//! Installs the pinned static FFmpeg build (`DependencyId::Ffmpeg`).

use std::path::Path;

use anyhow::{bail, Context, Result};

use super::{archive, download, paths, versions, StageProgressFn};

/// Downloads + extracts the pinned build into a fresh tmp dir, atomically
/// renamed into `runtime/ffmpeg/` only once `ffmpeg.exe`/`ffprobe.exe` are
/// confirmed present.
pub async fn install(app_data_dir: &Path, on_progress: StageProgressFn) -> Result<()> {
    tracing::info!("installing FFmpeg");
    let runtime_dir = paths::runtime_dir(app_data_dir);
    tokio::fs::create_dir_all(&runtime_dir).await?;
    // The zip and its extracted contents briefly coexist inside the tmp
    // dir before the zip is deleted, so budget roughly double the download
    // size as a safety margin.
    download::check_free_space(&runtime_dir, versions::FFMPEG_APPROX_DOWNLOAD_BYTES * 2)?;

    let tmp = paths::tmp_component_dir(app_data_dir, "ffmpeg");
    let result = try_install_into(&tmp, &on_progress).await;
    if let Err(e) = &result {
        tracing::error!("FFmpeg install failed: {e:#}");
        let _ = std::fs::remove_dir_all(&tmp);
        return result;
    }

    let final_dir = paths::ffmpeg_dir(app_data_dir);
    if final_dir.exists() {
        tokio::fs::remove_dir_all(&final_dir).await?;
    }
    tokio::fs::rename(&tmp, &final_dir)
        .await
        .with_context(|| format!("failed to move {tmp:?} into place at {final_dir:?}"))?;
    on_progress("done", 1.0);
    tracing::info!("FFmpeg installed");
    Ok(())
}

async fn try_install_into(tmp: &Path, on_progress: &StageProgressFn) -> Result<()> {
    tokio::fs::create_dir_all(tmp).await?;

    let archive_path = tmp.join("ffmpeg.zip");
    let client = reqwest::Client::new();
    {
        let progress = on_progress.clone();
        download::download_with_progress(&client, versions::FFMPEG_URL, &archive_path, move |p| {
            progress("downloading", p)
        })
        .await?;
    }
    download::verify_sha256(&archive_path, versions::FFMPEG_SHA256)?;

    let extracted = tmp.join("extracted");
    {
        let progress = on_progress.clone();
        let archive_for_task = archive_path.clone();
        let extracted_for_task = extracted.clone();
        tokio::task::spawn_blocking(move || {
            archive::extract_zip(&archive_for_task, &extracted_for_task, move |p| progress("extracting", p))
        })
        .await
        .context("ffmpeg extraction task panicked")??;
    }

    // The zip's sole top-level dir is named after the exact build (a
    // git-hash suffix that changes every release), so it's located
    // dynamically rather than hardcoded.
    let build_dir = archive::single_subdir(&extracted)?;
    let bin_dest = tmp.join("bin");
    tokio::fs::rename(build_dir.join("bin"), &bin_dest).await?;
    let _ = tokio::fs::remove_dir_all(&extracted).await;
    let _ = tokio::fs::remove_file(&archive_path).await;

    if !bin_dest.join("ffmpeg.exe").is_file() || !bin_dest.join("ffprobe.exe").is_file() {
        bail!("extracted ffmpeg build is missing ffmpeg.exe/ffprobe.exe at {bin_dest:?}");
    }

    paths::write_marker(&tmp.join(".installed"))?;
    Ok(())
}
