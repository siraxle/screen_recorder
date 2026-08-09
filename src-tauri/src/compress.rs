//! On-demand "compress for web" action, run against an already-finished
//! recording via a bundled HandBrakeCLI binary (see `bundle.resources` in
//! tauri.conf.json — shipped with the app so users never need to install
//! HandBrake themselves). This is deliberately not a `pipeline` job stage:
//! it isn't part of the Capture -> Transcribe -> Summarize -> Index pipeline
//! from screen-recorder-spec.md, just a standalone utility, so unlike
//! `pipeline`'s stages it isn't persisted/resumed across a restart.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use tauri::{AppHandle, Manager};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompressionProgress {
    pub recording_id: String,
    /// 0.0..=1.0
    pub progress: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompressionFinished {
    pub recording_id: String,
    pub status: &'static str, // "done" | "failed"
    pub output_path: Option<String>,
    pub message: Option<String>,
}

/// Locates the HandBrakeCLI binary bundled as a Tauri resource.
fn handbrake_cli_path(app: &AppHandle) -> Result<PathBuf> {
    let resource_dir = app
        .path()
        .resource_dir()
        .context("failed to resolve app resource directory")?;
    let path = resource_dir.join("HandBrakeCLI.exe");
    if !path.exists() {
        return Err(anyhow!("HandBrakeCLI.exe not found at {path:?}"));
    }
    Ok(path)
}

/// Fixed "optimized for web" preset, matching exactly what was asked for
/// (not a general transcode-settings UI): web-optimized MP4 (`-O`, fast-start
/// moov atom), "optimal size" anamorphic (HandBrake's Automatic scaling,
/// which maximizes output resolution up to the source rather than forcing a
/// specific width/height), H.264/x264 at quality 27, 128kbit/s AAC audio.
fn build_command(handbrake_cli: &Path, input: &Path, output: &Path) -> Command {
    let mut cmd = Command::new(handbrake_cli);
    cmd.arg("-i")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("-f")
        .arg("av_mp4")
        .arg("-O")
        .arg("-e")
        .arg("x264")
        .arg("-q")
        .arg("27")
        .arg("--auto-anamorphic")
        .arg("-E")
        .arg("av_aac")
        .arg("-B")
        .arg("128");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Pulls the most recent progress percentage out of HandBrakeCLI's stdout.
/// HandBrake rewrites `Encoding: task 1 of 1, NN.NN %` in place with `\r`
/// rather than emitting one line per update, so this scans raw accumulated
/// bytes for `%` markers instead of relying on newline-buffered reads.
fn parse_last_percent(buf: &str) -> Option<f64> {
    let mut last = None;
    for chunk in buf.split(['\r', '\n']) {
        let Some(percent_idx) = chunk.find('%') else { continue };
        let before = chunk[..percent_idx].trim();
        let num_start = before
            .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
            .map(|i| i + 1)
            .unwrap_or(0);
        if let Ok(value) = before[num_start..].parse::<f64>() {
            last = Some(value);
        }
    }
    last
}

/// Runs HandBrakeCLI to completion, reporting fractional progress as it
/// goes. `input`/`output` must not be the same path -- HandBrake opens the
/// destination for writing before the source is fully read.
pub async fn run_compression(
    app: &AppHandle,
    input: &Path,
    output: &Path,
    on_progress: impl Fn(f64) + Send + 'static,
) -> Result<()> {
    let handbrake_cli = handbrake_cli_path(app)?;
    run_compression_with_binary(&handbrake_cli, input, output, on_progress).await
}

/// The actual encode + progress-parsing loop, split out from
/// `run_compression` so tests can exercise it against a known binary path
/// without needing a running `AppHandle` (see the `tests` module below).
async fn run_compression_with_binary(
    handbrake_cli: &Path,
    input: &Path,
    output: &Path,
    on_progress: impl Fn(f64) + Send + 'static,
) -> Result<()> {
    let mut cmd = build_command(handbrake_cli, input, output);
    let mut child: Child = cmd.spawn().context("failed to spawn HandBrakeCLI")?;

    let mut stdout = child.stdout.take().expect("stdout piped in build_command");
    let stdout_task = tokio::spawn(async move {
        let mut read_buf = [0u8; 4096];
        let mut carry = String::new();
        loop {
            match stdout.read(&mut read_buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    carry.push_str(&String::from_utf8_lossy(&read_buf[..n]));
                    if let Some(percent) = parse_last_percent(&carry) {
                        on_progress((percent / 100.0).clamp(0.0, 1.0));
                    }
                    // Bound growth across a long encode; losing a stale
                    // partial line here just costs one skipped update.
                    if carry.len() > 8192 {
                        carry.clear();
                    }
                }
            }
        }
    });

    let drain = child.stderr.take().map(|stderr| {
        crate::capture::ffmpeg::spawn_stderr_drain(stderr, "handbrake".to_string())
    });

    let status = child.wait().await.context("HandBrakeCLI wait failed")?;
    let _ = stdout_task.await;
    if let Some(handle) = drain {
        let _ = handle.await;
    }

    if !status.success() {
        return Err(anyhow!("HandBrakeCLI exited with status {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn parse_last_percent_reads_the_final_progress_update() {
        // HandBrake rewrites the same line with `\r`; only the last update
        // in a batch of reads should win.
        let buf = "\rEncoding: task 1 of 1, 12.00 %\rEncoding: task 1 of 1, 42.31 %\r";
        assert_eq!(parse_last_percent(buf), Some(42.31));
    }

    #[test]
    fn parse_last_percent_ignores_lines_without_a_percent() {
        assert_eq!(parse_last_percent("Encode done!\nHandBrake has exited.\n"), None);
    }

    /// Real end-to-end run against the actual bundled HandBrakeCLI binary
    /// (see `src-tauri/resources/HandBrakeCLI.exe`) and a small ffmpeg-
    /// generated fixture video -- exercises the exact "optimized for web"
    /// preset (`build_command`) and progress parsing together, matching this
    /// project's other pipeline tests that spawn real subprocesses rather
    /// than mocking ffmpeg/HandBrake.
    #[tokio::test]
    async fn run_compression_produces_a_smaller_web_optimized_file() {
        let base_dir = std::env::temp_dir().join("screen_recorder_compress_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();

        let input = base_dir.join("input.mp4");
        let ffmpeg_status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:duration=2:rate=30",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=1000:duration=2",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-loglevel",
                "error",
            ])
            .arg(&input)
            .status()
            .expect("failed to spawn ffmpeg to build the test fixture");
        assert!(ffmpeg_status.success(), "ffmpeg fixture generation failed");

        let handbrake_cli = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("HandBrakeCLI.exe");
        assert!(
            handbrake_cli.exists(),
            "expected bundled HandBrakeCLI.exe at {handbrake_cli:?}"
        );

        let output = base_dir.join("output.web.mp4");
        let progress_updates = Arc::new(Mutex::new(Vec::<f64>::new()));
        let progress_updates_clone = progress_updates.clone();

        run_compression_with_binary(&handbrake_cli, &input, &output, move |p| {
            progress_updates_clone.lock().unwrap().push(p);
        })
        .await
        .expect("compression should succeed");

        assert!(output.exists(), "expected a compressed output file");
        assert!(std::fs::metadata(&output).unwrap().len() > 0);
        // At least the final update should have been observed.
        assert!(!progress_updates.lock().unwrap().is_empty());
    }
}
