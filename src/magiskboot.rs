use std::fs::File;
use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use tracing::{info, warn};

const MAGISK_RELEASE_API: &str = "https://api.github.com/repos/topjohnwu/Magisk/releases/latest";
const FALLBACK_APK_URL: &str =
    "https://github.com/topjohnwu/Magisk/releases/download/v27.0/Magisk-v27.0.apk";
const USER_AGENT: &str = "b2ak-rust/0.1 (+https://github.com/topjohnwu/Magisk)";

/// Downloads `magiskboot` from the latest Magisk APK if it is not already present.
pub async fn ensure(path: &Path, workspace: &Path) -> Result<()> {
    if path.exists() {
        info!("magiskboot already present at {}", path.display());
        return Ok(());
    }

    info!("magiskboot not found, downloading the latest Magisk APK");
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let apk_url = match latest_apk_url(&http).await {
        Ok(url) => url,
        Err(err) => {
            warn!("Failed to query the GitHub releases API ({err}); using fallback URL");
            FALLBACK_APK_URL.to_string()
        }
    };

    std::fs::create_dir_all(workspace)
        .with_context(|| format!("failed to create {}", workspace.display()))?;
    let temp = tempfile::Builder::new()
        .prefix("magisk-")
        .suffix(".apk")
        .tempfile_in(workspace)
        .context("failed to create a temporary APK file")?;

    info!("Downloading {apk_url}");
    download(&http, &apk_url, temp.path()).await?;

    let apk_path = temp.path().to_path_buf();
    let dest = path.to_path_buf();
    tokio::task::spawn_blocking(move || extract_magiskboot(&apk_path, &dest))
        .await
        .context("magiskboot extraction task crashed")??;

    info!("magiskboot extracted to {}", path.display());
    Ok(())
}

async fn latest_apk_url(http: &reqwest::Client) -> Result<String> {
    let response = http
        .get(MAGISK_RELEASE_API)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("GitHub API request failed")?
        .error_for_status()
        .context("GitHub API returned an error")?;

    let release: Value = response
        .json()
        .await
        .context("invalid JSON from the GitHub releases API")?;

    release
        .get("assets")
        .and_then(Value::as_array)
        .and_then(|assets| {
            assets.iter().find_map(|asset| {
                let name = asset.get("name").and_then(Value::as_str)?;
                if name.ends_with(".apk") {
                    asset
                        .get("browser_download_url")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| anyhow!("no .apk asset found in the latest Magisk release"))
}

async fn download(http: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let response = http
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .context("failed to download the Magisk APK")?
        .error_for_status()
        .context("Magisk APK download returned an error status")?;

    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("failed to create {}", dest.display()))?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed while reading the Magisk APK")?;
        file.write_all(&chunk)
            .await
            .context("failed while writing the Magisk APK")?;
    }
    file.flush().await.context("failed to flush the APK")?;
    Ok(())
}

fn extract_magiskboot(apk: &Path, dest: &Path) -> Result<()> {
    let file = File::open(apk).with_context(|| format!("failed to open {}", apk.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("the downloaded APK is not a zip file")?;

    let candidates = so_candidates();
    let name = candidates
        .iter()
        .find(|candidate| archive.file_names().any(|name| name == **candidate))
        .copied()
        .ok_or_else(|| {
            anyhow!(
                "magiskboot binary not found in the APK (tried: {})",
                candidates.join(", ")
            )
        })?;
    let mut entry = archive
        .by_name(name)
        .with_context(|| format!("failed to read {name} from the APK"))?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut out =
        File::create(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    io::copy(&mut entry, &mut out)
        .with_context(|| format!("failed to extract magiskboot to {}", dest.display()))?;
    drop(out);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .context("failed to mark magiskboot as executable")?;
    }

    Ok(())
}

fn so_candidates() -> Vec<&'static str> {
    let primary = match std::env::consts::ARCH {
        "x86_64" => Some("lib/x86_64/libmagiskboot.so"),
        "aarch64" => Some("lib/arm64-v8a/libmagiskboot.so"),
        "x86" => Some("lib/x86/libmagiskboot.so"),
        "arm" => Some("lib/armeabi-v7a/libmagiskboot.so"),
        _ => None,
    };

    let mut candidates = Vec::with_capacity(2);
    if let Some(primary) = primary {
        candidates.push(primary);
    }
    if !candidates.contains(&"lib/x86_64/libmagiskboot.so") {
        candidates.push("lib/x86_64/libmagiskboot.so");
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::so_candidates;

    #[test]
    fn candidates_are_never_empty() {
        assert!(!so_candidates().is_empty());
    }
}
