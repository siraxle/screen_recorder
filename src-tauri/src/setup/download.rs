//! Chunked HTTP download with progress, SHA256 verification, and a
//! Windows free-disk-space pre-check -- shared by `python_runtime` and
//! `ffmpeg_runtime`.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// Streams `url` to `dest`, calling `on_progress` with a 0.0..=1.0 fraction
/// after each chunk. If the server doesn't report `Content-Length`,
/// progress stays at 0.0 until the download completes (no way to estimate
/// a fraction of an unknown total), then a final 1.0 is reported.
pub async fn download_with_progress(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    mut on_progress: impl FnMut(f64),
) -> Result<()> {
    tracing::info!("downloading dependency from {url} to {dest:?}");
    let response = match client.get(url).send().await.with_context(|| format!("failed to request {url}")) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("dependency download request failed for {url}: {e:#}");
            return Err(e);
        }
    };
    let response = match response.error_for_status().with_context(|| format!("{url} returned an error status")) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("dependency download failed for {url}: {e:#}");
            return Err(e);
        }
    };
    let total = response.content_length();

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("failed to create {dest:?}"))?;

    let mut downloaded: u64 = 0;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk.with_context(|| format!("stream error downloading {url}")) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("dependency download stream error for {url}: {e:#}");
                return Err(e);
            }
        };
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        if let Some(total) = total {
            if total > 0 {
                on_progress((downloaded as f64 / total as f64).min(1.0));
            }
        }
    }
    file.flush().await?;
    on_progress(1.0);
    tracing::info!("finished downloading {url} ({downloaded} bytes) to {dest:?}");
    Ok(())
}

/// Verifies `path`'s SHA256 matches `expected_hex` (lowercase hex), erroring
/// with both hashes in the message so a mismatch is diagnosable without
/// re-running with extra logging.
pub fn verify_sha256(path: &Path, expected_hex: &str) -> Result<()> {
    let mut file = std::fs::File::open(path).with_context(|| format!("failed to open {path:?}"))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).with_context(|| format!("failed to hash {path:?}"))?;
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_hex) {
        tracing::error!("checksum mismatch for {path:?}: expected {expected_hex}, got {actual}");
        return Err(anyhow!(
            "checksum mismatch for {path:?}: expected {expected_hex}, got {actual}"
        ));
    }
    Ok(())
}

/// Errors with a clear message if fewer than `needed_bytes` are free on the
/// volume containing `dir` (`dir` must already exist). Windows-only, via
/// `GetDiskFreeSpaceExW` -- there's no portable std API for this.
#[cfg(target_os = "windows")]
pub fn check_free_space(dir: &Path, needed_bytes: u64) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut free_bytes_available: u64 = 0;
    // SAFETY: `wide` is a valid null-terminated UTF-16 string for the
    // lifetime of the call; the two size out-params are left null since we
    // only need the caller's available quota, not the volume totals.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_bytes_available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(anyhow!("failed to query free disk space for {dir:?}"));
    }
    if free_bytes_available < needed_bytes {
        let free_mb = free_bytes_available / (1024 * 1024);
        let needed_mb = needed_bytes / (1024 * 1024);
        return Err(anyhow!(
            "not enough free disk space at {dir:?}: {free_mb} MB free, ~{needed_mb} MB needed"
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn check_free_space(_dir: &Path, _needed_bytes: u64) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_sha256_accepts_matching_hash() {
        let path = std::env::temp_dir().join(format!("sha-test-ok-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"hello world").unwrap();
        // sha256("hello world")
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert!(verify_sha256(&path, expected).is_ok());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn verify_sha256_rejects_mismatch() {
        let path = std::env::temp_dir().join(format!("sha-test-bad-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"hello world").unwrap();
        let err = verify_sha256(&path, "0".repeat(64).as_str()).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn download_with_progress_writes_full_body_and_reaches_100_percent() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = b"a".repeat(10_000);
        Mock::given(method("GET"))
            .and(path("/file.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;

        let dest = std::env::temp_dir().join(format!("download-test-{}", uuid::Uuid::new_v4()));
        let client = reqwest::Client::new();
        let mut progress_values = Vec::new();
        download_with_progress(&client, &format!("{}/file.bin", server.uri()), &dest, |p| {
            progress_values.push(p);
        })
        .await
        .unwrap();

        let written = std::fs::read(&dest).unwrap();
        assert_eq!(written.len(), body.len());
        assert_eq!(*progress_values.last().unwrap(), 1.0);
        assert!(progress_values.windows(2).all(|w| w[0] <= w[1]));
        std::fs::remove_file(&dest).unwrap();
    }
}
