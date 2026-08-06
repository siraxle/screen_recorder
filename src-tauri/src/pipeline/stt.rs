use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{ChildStderr, Command};

use crate::db::Segment;

const DEFAULT_MODEL: &str = "small";

#[derive(Deserialize)]
struct WorkerSegment {
    start: f64,
    end: f64,
    text: String,
}

#[derive(Deserialize)]
struct WorkerOutput {
    language: String,
    segments: Vec<WorkerSegment>,
}

pub struct TranscriptionResult {
    pub language: String,
    pub segments: Vec<Segment>,
}

fn worker_script_path() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<repo>/src-tauri` at compile time, so this
    // resolves correctly regardless of the process's working directory
    // (`cargo tauri dev` vs. a packaged app).
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("python")
        .join("whisper_worker.py")
}

/// Reads a child's stderr to completion, logging each line via `tracing` and
/// returning the full text so a failure can surface the worker's traceback
/// as the job's error message.
async fn capture_stderr(stderr: ChildStderr) -> String {
    let mut lines = BufReader::new(stderr).lines();
    let mut collected = String::new();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                tracing::debug!(target: "whisper_worker", "{line}");
                collected.push_str(&line);
                collected.push('\n');
            }
            _ => break,
        }
    }
    collected
}

/// Spawns `python whisper_worker.py` on the given WAV file and waits for its
/// single JSON result. `language_override` of `None` lets the worker
/// auto-detect (native Whisper behavior).
pub async fn transcribe(wav_path: &Path, language_override: Option<&str>) -> Result<TranscriptionResult> {
    let mut cmd = Command::new("python");
    cmd.arg(worker_script_path())
        .arg("--audio")
        .arg(wav_path)
        .arg("--model")
        .arg(DEFAULT_MODEL)
        .arg("--device")
        .arg("cuda")
        .arg("--compute-type")
        .arg("float16");
    if let Some(lang) = language_override {
        cmd.arg("--language").arg(lang);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .context("failed to spawn whisper worker (is `python` on PATH with faster-whisper installed?)")?;

    let mut stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // Both stdout and stderr must be drained concurrently with `wait()`:
    // the worker's JSON result can exceed the OS pipe buffer, and reading
    // it only after the child exits would deadlock a large transcript.
    let stdout_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(capture_stderr(stderr));

    let status = child.wait().await.context("whisper worker wait failed")?;
    let stdout_output = stdout_task.await.context("stdout reader task panicked")?;
    let stderr_output = stderr_task.await.context("stderr reader task panicked")?;

    if !status.success() {
        let tail: Vec<&str> = stderr_output.lines().rev().take(20).collect();
        let tail: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(anyhow!("whisper worker exited with status {status}: {tail}"));
    }

    let parsed: WorkerOutput = serde_json::from_str(&stdout_output)
        .with_context(|| format!("failed to parse whisper worker output: {stdout_output}"))?;

    let segments = parsed
        .segments
        .into_iter()
        .map(|s| Segment {
            start_ms: (s.start * 1000.0).round() as i64,
            end_ms: (s.end * 1000.0).round() as i64,
            text: s.text,
        })
        .collect();

    Ok(TranscriptionResult {
        language: parsed.language,
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real (non-mocked) e2e test: runs the actual `whisper_worker.py`
    /// subprocess against a checked-in speech fixture (synthesized via
    /// Windows SAPI TTS, see `src-tauri/tests/fixtures/sample_en.wav`) on
    /// this dev machine's faster-whisper + CUDA setup, matching the
    /// capture module's existing testing philosophy. First run downloads
    /// the ~500MB "small" model from HuggingFace if not already cached.
    #[tokio::test]
    async fn transcribes_real_fixture_and_detects_language() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("sample_en.wav");
        assert!(fixture.exists(), "fixture missing at {fixture:?}");

        let result = transcribe(&fixture, None)
            .await
            .expect("transcription should succeed");

        assert_eq!(result.language, "en");
        assert!(!result.segments.is_empty(), "expected at least one segment");
        let full_text: String = result.segments.iter().map(|s| s.text.as_str()).collect();
        assert!(
            full_text.to_lowercase().contains("dog"),
            "expected the fixture's speech to appear in the transcript, got: {full_text:?}"
        );
    }

    /// A manually-specified language should be honored instead of
    /// auto-detection (F2's language override criterion).
    #[tokio::test]
    async fn honors_manual_language_override() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("sample_en.wav");

        let result = transcribe(&fixture, Some("en"))
            .await
            .expect("transcription should succeed");

        assert_eq!(result.language, "en");
    }
}
