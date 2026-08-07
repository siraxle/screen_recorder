use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout, Command};

use crate::db::{self, Segment};

pub(super) const DEFAULT_MODEL: &str = "small";

/// The two interchangeable STT backends (see CLAUDE.md: "runtime selected by
/// target hardware — never hardcode one runtime"). Both speak the same
/// NDJSON worker protocol, so everything downstream of `transcribe` is
/// engine-agnostic — only which subprocess gets spawned differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SttEngine {
    /// `faster-whisper` (CTranslate2): 99+ languages, auto-detects the
    /// spoken language per recording.
    Whisper,
    /// NVIDIA Parakeet-TDT-0.6B-v3 via the `onnx-asr` runtime: ~25 languages,
    /// far faster, but the model itself never reports a detected language
    /// (see `parakeet_command`'s doc comment).
    Parakeet,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SttSettings {
    pub engine: SttEngine,
}

impl Default for SttSettings {
    fn default() -> Self {
        Self { engine: SttEngine::Whisper }
    }
}

const KEY_STT_ENGINE: &str = "stt_engine";

/// Loads STT engine settings from the generic `settings` key/value table,
/// mirroring `llm::load_settings`'s fallback-to-default convention.
pub fn load_settings(conn: &Connection) -> Result<SttSettings> {
    let engine = match db::get_setting(conn, KEY_STT_ENGINE)?.as_deref() {
        Some("parakeet") => SttEngine::Parakeet,
        _ => SttEngine::Whisper,
    };
    Ok(SttSettings { engine })
}

pub fn save_settings(conn: &Connection, settings: &SttSettings) -> Result<()> {
    let engine_str = match settings.engine {
        SttEngine::Whisper => "whisper",
        SttEngine::Parakeet => "parakeet",
    };
    db::set_setting(conn, KEY_STT_ENGINE, engine_str)?;
    Ok(())
}

#[derive(Deserialize)]
struct WorkerSegment {
    start: f64,
    end: f64,
    text: String,
}

/// One line of the worker's newline-delimited stdout protocol (see
/// `python/whisper_worker.py`'s module docstring for the contract).
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerMessage {
    Progress {
        processed_seconds: f64,
        duration_seconds: f64,
    },
    Result {
        language: String,
        segments: Vec<WorkerSegment>,
    },
}

struct WorkerResult {
    language: String,
    segments: Vec<WorkerSegment>,
}

pub struct TranscriptionResult {
    pub language: String,
    pub segments: Vec<Segment>,
}

pub(super) fn python_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<repo>/src-tauri` at compile time, so this
    // resolves correctly regardless of the process's working directory
    // (`cargo tauri dev` vs. a packaged app).
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("python")
}

fn whisper_command(wav_path: &Path, language_override: Option<&str>) -> Command {
    let mut cmd = Command::new("python");
    cmd.arg(python_dir().join("whisper_worker.py"))
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
    cmd
}

/// Parakeet-TDT-0.6B-v3, run through the `onnx-asr` package rather than
/// NVIDIA's NeMo toolkit (NeMo is CUDA-only and far heavier; `onnx-asr` runs
/// the same checkpoint on CPU or CUDA via ONNX Runtime execution providers,
/// matching this app's per-hardware-swappable-runtime rule). Unlike Whisper,
/// the model itself takes no language hint and never reports one back — see
/// `python/parakeet_worker.py`'s doc comment — so `language_override` here is
/// purely a label for the `language` field the worker echoes back, not a
/// decoding constraint.
fn parakeet_command(wav_path: &Path, language_override: Option<&str>) -> Command {
    let mut cmd = Command::new("python");
    cmd.arg(python_dir().join("parakeet_worker.py"))
        .arg("--audio")
        .arg(wav_path)
        .arg("--device")
        .arg("cuda");
    if let Some(lang) = language_override {
        cmd.arg("--language").arg(lang);
    }
    cmd
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

/// Reads the worker's stdout line by line as it streams in, forwarding each
/// `progress` line's fraction (0.0..=1.0) to `on_progress` immediately and
/// keeping the last `result` line. Must run concurrently with `wait()`, like
/// `capture_stderr`, since the worker interleaves progress lines with the
/// final result rather than writing everything at once.
async fn capture_stdout(
    stdout: ChildStdout,
    on_progress: impl Fn(f64) + Send + 'static,
) -> Option<WorkerResult> {
    let mut lines = BufReader::new(stdout).lines();
    let mut result = None;
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match serde_json::from_str::<WorkerMessage>(&line) {
                Ok(WorkerMessage::Progress {
                    processed_seconds,
                    duration_seconds,
                }) => {
                    if duration_seconds > 0.0 {
                        on_progress((processed_seconds / duration_seconds).clamp(0.0, 1.0));
                    }
                }
                Ok(WorkerMessage::Result { language, segments }) => {
                    result = Some(WorkerResult { language, segments });
                }
                Err(_) => {
                    // Not a recognized protocol line (e.g. stray print from a
                    // dependency); ignore rather than failing the whole job,
                    // matching stderr's line-logging tolerance.
                }
            },
            _ => break,
        }
    }
    result
}

/// Spawns the worker subprocess for the given engine on the WAV file and
/// waits for its final JSON result, reporting progress fractions (0.0..=1.0)
/// via `on_progress` as segments are produced. `language_override` of `None`
/// lets Whisper auto-detect (native Whisper behavior); Parakeet has no
/// auto-detect (see `parakeet_command`).
pub async fn transcribe(
    wav_path: &Path,
    language_override: Option<&str>,
    engine: SttEngine,
    on_progress: impl Fn(f64) + Send + 'static,
) -> Result<TranscriptionResult> {
    let mut cmd = match engine {
        SttEngine::Whisper => whisper_command(wav_path, language_override),
        SttEngine::Parakeet => parakeet_command(wav_path, language_override),
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "failed to spawn {} worker (is `python` on PATH with its dependencies installed?)",
            match engine {
                SttEngine::Whisper => "whisper",
                SttEngine::Parakeet => "parakeet",
            }
        )
    })?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // Both stdout and stderr must be drained concurrently with `wait()`:
    // the worker's JSON result can exceed the OS pipe buffer, and reading
    // it only after the child exits would deadlock a large transcript.
    let stdout_task = tokio::spawn(capture_stdout(stdout, on_progress));
    let stderr_task = tokio::spawn(capture_stderr(stderr));

    let status = child.wait().await.context("stt worker wait failed")?;
    let result = stdout_task.await.context("stdout reader task panicked")?;
    let stderr_output = stderr_task.await.context("stderr reader task panicked")?;

    if !status.success() {
        let tail: Vec<&str> = stderr_output.lines().rev().take(20).collect();
        let tail: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(anyhow!("stt worker exited with status {status}: {tail}"));
    }

    let parsed = result.ok_or_else(|| {
        anyhow!("stt worker exited successfully but produced no result output")
    })?;

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

        let result = transcribe(&fixture, None, SttEngine::Whisper, |_| {})
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

    /// Progress lines the worker emits per-segment should reach the caller
    /// as fractions in 0.0..=1.0, ending at (or very near) complete.
    #[tokio::test]
    async fn reports_progress_during_transcription() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("sample_en.wav");

        let progress = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let progress_clone = progress.clone();

        transcribe(&fixture, None, SttEngine::Whisper, move |p| {
            progress_clone.lock().unwrap().push(p);
        })
        .await
        .expect("transcription should succeed");

        let progress = progress.lock().unwrap();
        assert!(!progress.is_empty(), "expected at least one progress update");
        assert!(
            progress.iter().all(|&p| (0.0..=1.0).contains(&p)),
            "all progress values should be within 0.0..=1.0, got: {progress:?}"
        );
        assert!(
            *progress.last().unwrap() > 0.9,
            "expected the final progress update to be near-complete, got: {progress:?}"
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

        let result = transcribe(&fixture, Some("en"), SttEngine::Whisper, |_| {})
            .await
            .expect("transcription should succeed");

        assert_eq!(result.language, "en");
    }
}
