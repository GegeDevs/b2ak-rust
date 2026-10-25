use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

use anyhow::{bail, Context, Result};
use grammers_client::message::InputMessage;
use grammers_client::Client;
use tokio::process::Command;
use tracing::{info, warn};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::config::Config;
use crate::magiskboot;

/// Runs the full boot.img -> AnyKernel zip pipeline and delivers the result.
///
/// The caller owns `temp_dir` and is responsible for deleting it afterwards
/// (in practice it is a `tempfile::TempDir` that cleans itself up on drop).
pub async fn process(
    client: &Client,
    config: &Config,
    message: &grammers_client::message::Message,
    temp_dir: &Path,
    status: &grammers_client::message::Message,
    remaining: u32,
    show_remaining: bool,
) -> Result<()> {
    edit_status(status, "🔧 Verifying magiskboot tool...").await;

    magiskboot::ensure(&config.magiskboot_path, &config.workspace_dir)
        .await
        .context("failed to retrieve or prepare the `magiskboot` binary")?;

    let boot_img = temp_dir.join("boot.img");
    if !boot_img.exists() {
        bail!("Boot image was not found in the temporary workspace.");
    }

    edit_status(status, "📦 Unpacking boot image...").await;
    let unpack = Command::new(&config.magiskboot_path)
        .arg("unpack")
        .arg("boot.img")
        .current_dir(temp_dir)
        .output()
        .await
        .with_context(|| format!("failed to execute {}", config.magiskboot_path.display()))?;

    if !unpack.status.success() {
        let stderr = String::from_utf8_lossy(&unpack.stderr);
        let stdout = String::from_utf8_lossy(&unpack.stdout);
        let details = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!(
            "Failed to unpack the boot image!\n\nError details:\n`{}`",
            truncate(details, 300)
        );
    }

    let kernel_path = temp_dir.join("kernel");
    if !kernel_path.exists() {
        bail!(
            "Kernel was not found in the unpacked boot image. Make sure you uploaded a valid boot.img."
        );
    }

    let image_path = temp_dir.join("Image");
    tokio::fs::rename(&kernel_path, &image_path)
        .await
        .context("failed to rename the extracted kernel to Image")?;

    edit_status(status, "🗂️ Cloning AnyKernel repository...").await;
    let anykernel_dir = temp_dir.join("AnyKernel");
    let clone = Command::new("git")
        .arg("clone")
        .arg("--depth")
        .arg("1")
        .arg("-b")
        .arg(&config.anykernel_branch)
        .arg(&config.anykernel_repo)
        .arg(&anykernel_dir)
        .output()
        .await
        .context("failed to execute git (is git installed?)")?;

    if !clone.status.success() {
        let stderr = String::from_utf8_lossy(&clone.stderr);
        let stdout = String::from_utf8_lossy(&clone.stdout);
        let details = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!(
            "Failed to clone the AnyKernel repository!\n\nError details:\n`{}`",
            truncate(details, 300)
        );
    }

    let destination_image = anykernel_dir.join("Image");
    tokio::fs::copy(&image_path, &destination_image)
        .await
        .with_context(|| format!("failed to copy Image into {}", destination_image.display()))?;
    info!(
        "Replaced Image in AnyKernel: {}",
        destination_image.display()
    );

    let git_dir = anykernel_dir.join(".git");
    match tokio::fs::remove_dir_all(&git_dir).await {
        Ok(()) => info!("Removed .git directory from AnyKernel before archiving"),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => warn!("Failed to remove {}: {err}", git_dir.display()),
    }

    edit_status(status, "🤐 Creating flashable zip archive...").await;
    let zip_path = temp_dir.join("AnyKernel_Flashable.zip");
    let archive_source = anykernel_dir.clone();
    let archive_dest = zip_path.clone();
    tokio::task::spawn_blocking(move || create_zip(&archive_source, &archive_dest))
        .await
        .context("zip creation task crashed")?
        .context("failed to package the zip archive")?;

    if !zip_path.exists() {
        bail!("Failed to package the zip archive.");
    }

    edit_status(status, "📤 Uploading zip archive...").await;
    let uploaded = client
        .upload_file(&zip_path)
        .await
        .context("failed to upload the flashable zip to Telegram")?;

    let mut caption = String::from("✅ **AnyKernel Flashable Zip compiled successfully!**");
    if show_remaining {
        caption.push_str(&format!("\n📊 *Remaining runs today: {remaining}*"));
    }

    message
        .reply(InputMessage::new().markdown(caption).document(uploaded))
        .await
        .context("failed to send the flashable zip to the chat")?;

    let _ = status.delete().await;
    info!("Job completed successfully");
    Ok(())
}

async fn edit_status(status: &grammers_client::message::Message, text: &str) {
    if let Err(err) = status.edit(InputMessage::new().text(text)).await {
        warn!("Failed to edit the status message: {err}");
    }
}

fn create_zip(source: &Path, destination: &Path) -> Result<()> {
    let file = File::create(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let mut writer = ZipWriter::new(file);

    let mut pending = vec![source.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .with_context(|| format!("failed to read {}", directory.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;

            if metadata.file_type().is_symlink() {
                // Follow file symlinks (like Python's zipfile does) but never
                // recurse through directory symlinks, which could loop.
                let target = std::fs::metadata(&path)?;
                if target.is_file() {
                    add_file(&mut writer, source, &path, &target)?;
                }
                continue;
            }

            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                add_file(&mut writer, source, &path, &metadata)?;
            }
        }
    }

    writer.finish().context("failed to finalize the zip")?;
    Ok(())
}

fn add_file(
    writer: &mut ZipWriter<File>,
    source: &Path,
    path: &Path,
    metadata: &Metadata,
) -> Result<()> {
    let relative = path
        .strip_prefix(source)
        .with_context(|| format!("{} is not inside {}", path.display(), source.display()))?;
    let name = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");

    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(unix_mode(metadata));

    writer
        .start_file(name, options)
        .with_context(|| format!("failed to add {} to the zip", path.display()))?;
    let mut input =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    io::copy(&mut input, writer)
        .with_context(|| format!("failed to compress {}", path.display()))?;
    Ok(())
}

fn unix_mode(metadata: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0o644
    }
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// Creates a zip archive with all files at the root level, like
/// `shutil.make_archive(base, "zip", anykernel_dir)` does.
#[cfg(test)]
pub fn archive_root(source: &Path, destination: &Path) -> Result<()> {
    create_zip(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn archive_contains_files_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("anykernel.sh"), b"#!/sbin/sh\n").unwrap();
        std::fs::write(source.join("nested/Image"), b"kernel").unwrap();

        let zip_path = dir.path().join("out.zip");
        archive_root(&source, &zip_path).unwrap();

        let file = File::open(&zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut names: Vec<String> = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["anykernel.sh", "nested/Image"]);

        let mut contents = String::new();
        archive
            .by_name("anykernel.sh")
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents, "#!/sbin/sh\n");
    }
}
