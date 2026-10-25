use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use grammers_client::client::UpdateStream;
use grammers_client::media::Media;
use grammers_client::message::{InputMessage, Message};
use grammers_client::update::Update;
use grammers_client::Client;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::gofile;
use crate::repack;
use crate::usage::{UsageStore, DAILY_LIMIT};

const WELCOME: &str = "🤖 **Boot.img to AnyKernel Flashable Zip Bot**\n\n\
Send me a `boot.img` file as a document **OR** send a direct download link \
starting with `http` or `https`.\n\n\
📌 **How to use:**\n\
1. Upload your kernel's `boot.img` as a document OR paste its direct download link.\n\
2. **Gofile.io links** (`https://gofile.io/d/...`) are also supported — add the password \
after the URL if the content is protected, e.g. `https://gofile.io/d/abc123 mypassword`.\n\
3. The bot will automatically download, unpack it, clone AnyKernel, swap the kernel, and pack it.\n\
4. You will receive the compiled flashable zip file in seconds.\n\n\
⚠️ **Note**: Users are limited to **3 compiles per day** (bot owner is exempt).";

const LIMIT_EXCEEDED: &str =
    "🛑 **Limit Exceeded!** You have reached your daily limit of **3 compiles per day**.";

pub struct AppState {
    pub client: Client,
    pub config: Arc<Config>,
    pub usage: UsageStore,
    pub http: reqwest::Client,
    pub semaphore: Arc<Semaphore>,
}

/// Consumes Telegram updates forever. Each message is handled in its own task,
/// bounded by a semaphore so a burst of uploads cannot fill the disk at once.
pub async fn run(state: Arc<AppState>, updates: &mut UpdateStream) -> Result<()> {
    loop {
        let update = match updates.next().await {
            Ok(update) => update,
            Err(err) => {
                warn!("Failed to receive updates: {err}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let Update::NewMessage(message) = update else {
            continue;
        };
        if message.outgoing() {
            continue;
        }

        let state = state.clone();
        let semaphore = state.semaphore.clone();
        tokio::spawn(async move {
            let _permit = match semaphore.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return,
            };
            if let Err(err) = handle_message(&state, &message).await {
                error!("Failed to handle message {}: {err:#}", message.id());
            }
        });
    }
}

async fn handle_message(state: &Arc<AppState>, message: &Message) -> Result<()> {
    if message.media().is_some() {
        return handle_document(state, message).await;
    }

    let text = message.text().trim();
    if text.is_empty() {
        return Ok(());
    }

    if let Some(command) = command_name(text) {
        if matches!(command, "start" | "help") {
            message
                .reply(InputMessage::new().markdown(WELCOME))
                .await
                .context("failed to send the welcome message")?;
            return Ok(());
        }
    }

    handle_link(state, message, text).await
}

async fn handle_document(state: &Arc<AppState>, message: &Message) -> Result<()> {
    let Some(Media::Document(document)) = message.media() else {
        // The original bot only reacts to documents; photos and other media are ignored.
        return Ok(());
    };

    let file_name = document.name().unwrap_or("").to_string();
    if !file_name.to_ascii_lowercase().ends_with(".img") {
        message
            .reply("❌ Invalid file! Please upload a `.img` file (e.g., `boot.img`).")
            .await
            .context("failed to reject the uploaded file")?;
        return Ok(());
    }

    let Some(user_id) = user_id(message) else {
        return Ok(());
    };

    let (allowed, count) = state
        .usage
        .check_and_increment(user_id, state.config.owner_id)
        .await;
    if !allowed {
        message
            .reply(InputMessage::new().markdown(LIMIT_EXCEEDED))
            .await
            .context("failed to send the limit message")?;
        return Ok(());
    }

    let temp_dir = tempfile::Builder::new()
        .prefix("b2ak-")
        .tempdir_in(&state.config.workspace_dir)
        .context("failed to create a temporary workspace")?;
    let boot_path = temp_dir.path().join("boot.img");

    let status = message
        .reply("⏳ Downloading your boot image...")
        .await
        .context("failed to send the download status message")?;

    match state.client.download_media(&document, &boot_path).await {
        Ok(()) => {}
        Err(err) => {
            edit_status(
                &status,
                format!("❌ Failed to download file from Telegram: {err}"),
            )
            .await;
            return Ok(());
        }
    }

    run_pipeline(state, message, temp_dir.path(), &status, count).await;
    Ok(())
}

async fn handle_link(state: &Arc<AppState>, message: &Message, text: &str) -> Result<()> {
    let mut tokens = text.split_whitespace();
    let url = tokens.next().unwrap_or("");
    let password = tokens.next().map(str::to_string);

    if !(url.starts_with("http://") || url.starts_with("https://")) {
        message
            .reply(
                "❌ Please send a valid direct download link starting with `http://` or `https://`.",
            )
            .await
            .context("failed to reject an invalid link")?;
        return Ok(());
    }

    let Some(user_id) = user_id(message) else {
        return Ok(());
    };

    let (allowed, count) = state
        .usage
        .check_and_increment(user_id, state.config.owner_id)
        .await;
    if !allowed {
        message
            .reply(InputMessage::new().markdown(LIMIT_EXCEEDED))
            .await
            .context("failed to send the limit message")?;
        return Ok(());
    }

    let is_gofile = url.contains("gofile.io");
    let source_label = if is_gofile { "gofile.io" } else { "link" };

    let temp_dir = tempfile::Builder::new()
        .prefix("b2ak-")
        .tempdir_in(&state.config.workspace_dir)
        .context("failed to create a temporary workspace")?;
    let boot_path = temp_dir.path().join("boot.img");

    let status = message
        .reply(format!("⏳ Downloading boot image from {source_label}..."))
        .await
        .context("failed to send the download status message")?;

    let download = if is_gofile {
        gofile::download_bootimg(
            url,
            &boot_path,
            password.as_deref(),
            state.config.gf_token.as_deref(),
            state.config.gf_timeout,
        )
        .await
    } else {
        download_direct(&state.http, url, &boot_path).await
    };

    if let Err(err) = download {
        error!("Failed to download from link {url}: {err:#}");
        let hint = if is_gofile {
            "Check that the gofile.io link is still public. If the content is password \
             protected, send the password right after the URL."
        } else {
            "The link must point directly to a .img file; preview pages and \
             login-protected links cannot be downloaded."
        };
        edit_status(
            &status,
            format!(
                "❌ Could not process the link\n\n\
                 🔗 {url}\n\n\
                 📄 Reason: {reason}\n\n\
                 {hint}\n\n\
                 💡 You can also upload the boot.img file directly to this chat as a document.",
                reason = truncate(&format!("{err:#}"), 500),
            ),
        )
        .await;
        return Ok(());
    }

    match tokio::fs::metadata(&boot_path).await {
        Ok(metadata) if metadata.len() > 0 => {}
        _ => {
            edit_status(
                &status,
                "❌ Downloaded file is empty or could not be retrieved.",
            )
            .await;
            return Ok(());
        }
    }

    run_pipeline(state, message, temp_dir.path(), &status, count).await;
    Ok(())
}

async fn run_pipeline(
    state: &Arc<AppState>,
    message: &Message,
    temp_dir: &Path,
    status: &Message,
    count: u32,
) {
    let show_remaining = user_id(message) != Some(state.config.owner_id);
    let remaining = DAILY_LIMIT.saturating_sub(count);
    info!("Starting boot.img pipeline (remaining runs: {remaining}, show: {show_remaining})");

    if let Err(err) = repack::process(
        &state.client,
        &state.config,
        message,
        temp_dir,
        status,
        remaining,
        show_remaining,
    )
    .await
    {
        error!("Boot image processing failed: {err:#}");
        edit_status(status, format!("❌ {err:#}")).await;
    }
}

async fn download_direct(http: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    let response = http
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        )
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                anyhow::anyhow!("the download server did not respond in time (timeout)")
            } else if err.is_connect() {
                anyhow::anyhow!("could not connect to the download server")
            } else {
                anyhow::anyhow!("the download request failed: {err}")
            }
        })?;

    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("the server answered with HTTP {status}");
    }

    // A preview page (Google Drive, Dropbox, ...) returns HTML with HTTP 200.
    if let Some(content_type) = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        let content_type = content_type.to_ascii_lowercase();
        if content_type.starts_with("text/html") || content_type.starts_with("application/json") {
            anyhow::bail!(
                "the link returned {content_type} instead of a file, so it is probably a \
                 preview page rather than a direct download link"
            );
        }
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("failed to create {}", dest.display()))?;
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed while reading the download")?;
        file.write_all(&chunk)
            .await
            .context("failed while writing the downloaded file")?;
        downloaded += chunk.len() as u64;
    }
    file.flush().await.context("failed to flush the file")?;
    info!("Downloaded {downloaded} bytes from {url}");
    Ok(())
}

async fn edit_status(status: &Message, text: impl Into<String>) {
    let _ = status.edit(InputMessage::new().text(text.into())).await;
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut truncated: String = value.chars().take(max).collect();
    truncated.push('…');
    truncated
}

fn user_id(message: &Message) -> Option<i64> {
    message.sender_id().and_then(|peer| peer.bare_id())
}

fn command_name(text: &str) -> Option<&str> {
    let first = text.split_whitespace().next()?;
    let command = first.strip_prefix('/')?;
    Some(command.split('@').next().unwrap_or(command))
}
