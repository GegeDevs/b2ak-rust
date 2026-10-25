use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// Runtime configuration, read from environment variables (and an optional `.env` file).
#[derive(Debug, Clone)]
pub struct Config {
    pub api_id: i32,
    pub api_hash: String,
    pub bot_token: String,
    pub owner_id: i64,

    pub anykernel_repo: String,
    pub anykernel_branch: String,

    pub gf_token: Option<String>,
    pub gf_timeout: Duration,

    pub workspace_dir: PathBuf,
    pub magiskboot_path: PathBuf,
    pub session_path: PathBuf,
    pub usage_path: PathBuf,

    pub max_concurrent_jobs: usize,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let bot_token = required("TELEGRAM_BOT_TOKEN")?;
        let api_hash = required("TELEGRAM_API_HASH")?;
        let api_id: i32 = required("TELEGRAM_API_ID")?
            .parse()
            .context("TELEGRAM_API_ID must be a valid integer")?;

        let owner_id = optional("OWNER_ID")
            .unwrap_or_else(|| "0".to_string())
            .parse()
            .context("OWNER_ID must be a numeric Telegram user ID")?;

        // Optional variables may be present but empty (for example, an unset
        // GitHub Actions secret is exposed as an empty string).
        let anykernel_repo = optional("ANYKERNEL_REPO")
            .unwrap_or_else(|| "https://github.com/osm0sis/AnyKernel3.git".to_string());
        let anykernel_branch = optional("ANYKERNEL_BRANCH").unwrap_or_else(|| "master".to_string());

        let gf_token = optional("GF_TOKEN");
        let gf_timeout_secs: f64 = optional("GF_TIMEOUT")
            .unwrap_or_else(|| "15.0".to_string())
            .parse()
            .context("GF_TIMEOUT must be a number of seconds")?;
        if !gf_timeout_secs.is_finite() || gf_timeout_secs <= 0.0 {
            bail!("GF_TIMEOUT must be a positive number of seconds");
        }

        let workspace_dir = absolute_workspace()?;
        std::fs::create_dir_all(&workspace_dir).with_context(|| {
            format!(
                "failed to create workspace directory {}",
                workspace_dir.display()
            )
        })?;

        let magiskboot_path = optional("MAGISKBOOT_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_dir.join("magiskboot"));
        let magiskboot_path = absolutize(&magiskboot_path, &workspace_dir);

        let session_path = optional("SESSION_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_dir.join("repacker_bot.session"));
        let session_path = absolutize(&session_path, &workspace_dir);
        if let Some(parent) = session_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create the session directory {}",
                    parent.display()
                )
            })?;
        }

        let usage_path = workspace_dir.join("usage.json");

        let max_concurrent_jobs: usize = optional("MAX_CONCURRENT_JOBS")
            .unwrap_or_else(|| "4".to_string())
            .parse()
            .context("MAX_CONCURRENT_JOBS must be a positive integer")?;

        Ok(Self {
            api_id,
            api_hash,
            bot_token,
            owner_id,
            anykernel_repo,
            anykernel_branch,
            gf_token,
            gf_timeout: Duration::from_secs_f64(gf_timeout_secs),
            workspace_dir,
            magiskboot_path,
            session_path,
            usage_path,
            max_concurrent_jobs: max_concurrent_jobs.max(1),
        })
    }
}

fn required(key: &str) -> Result<String> {
    match optional(key) {
        Some(value) => Ok(value),
        None => bail!("{key} is missing"),
    }
}

fn optional(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn absolute_workspace() -> Result<PathBuf> {
    let raw = optional("WORKSPACE_DIR").unwrap_or_else(|| ".".to_string());
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .context("failed to resolve the current directory")?
            .join(path)
    };
    Ok(std::fs::canonicalize(&path).unwrap_or(path))
}

fn absolutize(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}
