//! F6 live subtitles: a second, ephemeral STT path that runs alongside an
//! in-progress recording, producing display-only captions -- not persisted
//! to the DB, not merged into the F2 batch transcript, which remains the
//! sole source of truth for saved transcripts/subtitles. Scoped (by user
//! decision) to system audio only, Windows only, faster-whisper only; see
//! screen-recorder-spec.md F6 and CLAUDE.md's precedent for OS-gapped
//! features (Wayland, etc.) for why the other combinations aren't wired up.
//!
//! Pipeline: `capture::system_audio::LoopbackCapture` best-effort forwards a
//! clone of every captured PCM chunk into the sender returned by `start`;
//! that feeds an ffmpeg resampler (48kHz stereo -> 16kHz mono,
//! `ffmpeg::build_resample_command`), whose output is chopped into fixed
//! ~2.5s windows and sent one at a time (behind the same GPU thermal guard
//! used for LLM calls) to `python/whisper_worker.py --live`, a daemon that
//! loads the model once and transcribes each window independently. Captions
//! come back over the `live-subtitle-caption` Tauri event.
//!
//! No `recording_id` on that event (or `live-subtitle-cooldown`): unlike
//! the post-stop pipeline events, no DB recording id exists yet while a
//! recording is in progress -- `commands::stop_recording` only mints one at
//! the very end -- and this app supports exactly one active recording at a
//! time (`state::AppState`), so there's nothing to correlate against.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::capture::ffmpeg;
use crate::db;

use super::stt::{self, python_dir, DeviceSetting, DEFAULT_MODEL};

const KEY_LIVE_SUBTITLES_ENABLED: &str = "live_subtitles_enabled";

/// 16kHz mono f32le, ~2.5s per window (40,000 samples * 4 bytes). A fixed
/// literal rather than a computed expression so the byte count is
/// unambiguous at a glance; the derivation is spelled out here instead.
/// Shorter windows lower caption latency at the cost of choppier,
/// more-fragmented text (each window is transcribed with no context from
/// the last); 2.5s is a starting point for the spec's "~2-3s" target, not a
/// tuned constant.
const LIVE_WINDOW_BYTES: usize = 160_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSubtitlesSettings {
    pub enabled: bool,
}

impl Default for LiveSubtitlesSettings {
    fn default() -> Self {
        Self { enabled: false }
    }
}

/// Mirrors `stt::load_settings`'s fallback-to-default convention.
pub fn load_settings(conn: &Connection) -> Result<LiveSubtitlesSettings> {
    let enabled = db::get_setting(conn, KEY_LIVE_SUBTITLES_ENABLED)?.as_deref() == Some("true");
    Ok(LiveSubtitlesSettings { enabled })
}

pub fn save_settings(conn: &Connection, settings: &LiveSubtitlesSettings) -> Result<()> {
    db::set_setting(
        conn,
        KEY_LIVE_SUBTITLES_ENABLED,
        if settings.enabled { "true" } else { "false" },
    )
}

/// Payload for `live-subtitle-caption`. `text: ""` means "clear the
/// caption" (see `whisper_worker.py --live`'s module doc for when that's
/// sent).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveSubtitleCaption {
    text: String,
}

/// Payload for `live-subtitle-cooldown`, emitted while a window is held
/// back by `thermal::wait_for_safe_temp`. Deliberately a separate event
/// from the existing `thermal-cooldown` (LLM calls) rather than overloading
/// it with a placeholder `recording_id`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveSubtitleCooldown {
    gpu_temp_c: u32,
}

/// One line of `whisper_worker.py --live`'s stdout protocol.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LiveWorkerMessage {
    Caption { text: String },
}

/// Buffers arbitrary-sized PCM byte chunks and yields complete
/// `LIVE_WINDOW_BYTES`-sized windows as soon as enough bytes have
/// accumulated. Pure/IO-free so it's unit-testable without spawning any
/// subprocess (mirrors `thermal::parse_temp_output`'s split of pure logic
/// from IO).
#[derive(Default)]
struct WindowAccumulator {
    buf: Vec<u8>,
}

impl WindowAccumulator {
    fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(chunk);
        let mut windows = Vec::new();
        while self.buf.len() >= LIVE_WINDOW_BYTES {
            windows.push(self.buf.drain(..LIVE_WINDOW_BYTES).collect());
        }
        windows
    }
}

fn build_live_worker_command(language_override: Option<&str>, device: &str) -> Command {
    let mut cmd = Command::new(crate::setup::paths::python_program());
    cmd.arg(python_dir().join("whisper_worker.py"))
        .arg("--live")
        .arg("--model")
        .arg(DEFAULT_MODEL)
        .arg("--device")
        .arg(device)
        .arg("--compute-type")
        .arg(stt::compute_type_for(device));
    if let Some(lang) = language_override {
        cmd.arg("--language").arg(lang);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// Owns the whole live-captioning pipeline for one recording session:
/// the resampler and daemon subprocesses, plus the three background tasks
/// wiring them together. One instance lives in `AppState::live_subtitles`
/// for the duration of a recording, mirroring `AppState::controller`.
pub struct LiveSubtitles {
    resampler: Child,
    daemon: Child,
    tasks: Vec<JoinHandle<()>>,
}

impl LiveSubtitles {
    /// Spawns the resampler and daemon and wires up the pump/window/caption
    /// tasks. Returns the sender end of the audio tap channel -- the caller
    /// hands it to `CaptureController` so `LoopbackCapture` can forward
    /// captured PCM into it every segment. Dropping every clone of that
    /// sender (which `CaptureController` does when it's itself dropped) is
    /// what starts the pipeline's graceful EOF-driven shutdown; `stop`
    /// below handles the rest.
    pub async fn start(
        app_handle: AppHandle,
        language_override: Option<String>,
        device_pref: DeviceSetting,
    ) -> Result<(Self, mpsc::UnboundedSender<Vec<u8>>)> {
        let device = stt::resolve_device(device_pref).await;
        let mut resampler = ffmpeg::build_resample_command()
            .kill_on_drop(true)
            .spawn()
            .context("failed to spawn live-subtitles resampler")?;
        let mut resampler_stdin = resampler.stdin.take().expect("resampler stdin was piped");
        let mut resampler_stdout = resampler.stdout.take().expect("resampler stdout was piped");
        if let Some(stderr) = resampler.stderr.take() {
            ffmpeg::spawn_stderr_drain(stderr, "live-subtitles-resample".to_string());
        }

        let mut daemon = build_live_worker_command(language_override.as_deref(), device)
            .spawn()
            .context("failed to spawn live whisper worker (is the bundled Python runtime installed? see Settings \u{2192} Dependencies)")?;
        let mut daemon_stdin = daemon.stdin.take().expect("daemon stdin was piped");
        let daemon_stdout = daemon.stdout.take().expect("daemon stdout was piped");
        if let Some(stderr) = daemon.stderr.take() {
            ffmpeg::spawn_stderr_drain(stderr, "live-subtitles-whisper".to_string());
        }

        let (tap_tx, mut tap_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // Tap channel -> resampler stdin. Dropping `resampler_stdin` once
        // the channel closes is what signals the resampler to flush + exit.
        let pump_task = tokio::spawn(async move {
            while let Some(bytes) = tap_rx.recv().await {
                if resampler_stdin.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });

        // Resampler stdout -> fixed windows -> thermal gate -> daemon stdin
        // frames. Dropping `daemon_stdin` on exit propagates EOF onward.
        let cooldown_app_handle = app_handle.clone();
        let window_task = tokio::spawn(async move {
            let on_cooldown: Box<dyn Fn(u32) + Send + Sync> = Box::new(move |gpu_temp_c: u32| {
                let _ = cooldown_app_handle.emit(
                    "live-subtitle-cooldown",
                    LiveSubtitleCooldown { gpu_temp_c },
                );
            });
            let mut acc = WindowAccumulator::default();
            let mut read_buf = [0u8; 65536];
            loop {
                let n = match resampler_stdout.read(&mut read_buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                for window in acc.push(&read_buf[..n]) {
                    crate::thermal::wait_for_safe_temp(on_cooldown.as_ref()).await;
                    let len = window.len() as u32;
                    if daemon_stdin.write_all(&len.to_le_bytes()).await.is_err() {
                        return;
                    }
                    if daemon_stdin.write_all(&window).await.is_err() {
                        return;
                    }
                }
            }
        });

        // Daemon stdout -> NDJSON captions -> Tauri event.
        let caption_app_handle = app_handle;
        let caption_task = tokio::spawn(async move {
            let mut lines = BufReader::new(daemon_stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Ok(LiveWorkerMessage::Caption { text }) =
                            serde_json::from_str::<LiveWorkerMessage>(&line)
                        {
                            let _ = caption_app_handle
                                .emit("live-subtitle-caption", LiveSubtitleCaption { text });
                        }
                    }
                    _ => break,
                }
            }
        });

        Ok((
            Self {
                resampler,
                daemon,
                tasks: vec![pump_task, window_task, caption_task],
            },
            tap_tx,
        ))
    }

    /// Best-effort shutdown: waits briefly for the pipeline to drain via
    /// EOF propagation (started by the caller dropping its tap sender
    /// before calling this), then hard-kills anything still alive so a
    /// live-subtitles hang can never block a recording from stopping.
    pub async fn stop(mut self) {
        const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

        for task in self.tasks.drain(..) {
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, task).await;
        }
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, self.resampler.wait())
            .await
            .is_err()
        {
            let _ = self.resampler.kill().await;
        }
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, self.daemon.wait())
            .await
            .is_err()
        {
            let _ = self.daemon.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_accumulator_yields_no_windows_below_threshold() {
        let mut acc = WindowAccumulator::default();
        let windows = acc.push(&vec![0u8; LIVE_WINDOW_BYTES - 1]);
        assert!(windows.is_empty());
    }

    #[test]
    fn window_accumulator_yields_exactly_one_window_at_threshold() {
        let mut acc = WindowAccumulator::default();
        let windows = acc.push(&vec![7u8; LIVE_WINDOW_BYTES]);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].len(), LIVE_WINDOW_BYTES);
        assert!(windows[0].iter().all(|&b| b == 7));
    }

    #[test]
    fn window_accumulator_carries_remainder_across_pushes() {
        let mut acc = WindowAccumulator::default();
        let first = acc.push(&vec![1u8; LIVE_WINDOW_BYTES + 100]);
        assert_eq!(first.len(), 1, "one full window, 100 bytes held back");

        let second = acc.push(&vec![2u8; LIVE_WINDOW_BYTES - 100]);
        assert_eq!(second.len(), 1, "remainder + new bytes complete a second window");
        assert_eq!(&second[0][..100], &vec![1u8; 100][..]);
        assert_eq!(&second[0][100..], &vec![2u8; LIVE_WINDOW_BYTES - 100][..]);
    }

    #[test]
    fn window_accumulator_yields_multiple_windows_from_one_large_push() {
        let mut acc = WindowAccumulator::default();
        let windows = acc.push(&vec![9u8; LIVE_WINDOW_BYTES * 3 + 50]);
        assert_eq!(windows.len(), 3);
        assert_eq!(acc.buf.len(), 50);
    }

    #[tokio::test]
    async fn settings_round_trip_through_the_kv_store() {
        let base_dir = std::env::temp_dir().join("screen_recorder_live_subtitles_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();
        let conn_pool = db::open(&base_dir.join("db.sqlite3")).unwrap();

        let loaded = db::with_db(&conn_pool, |conn| load_settings(conn)).await.unwrap();
        assert_eq!(loaded, LiveSubtitlesSettings { enabled: false }, "default is off");

        db::with_db(&conn_pool, |conn| {
            save_settings(conn, &LiveSubtitlesSettings { enabled: true })
        })
        .await
        .unwrap();
        let loaded = db::with_db(&conn_pool, |conn| load_settings(conn)).await.unwrap();
        assert_eq!(loaded, LiveSubtitlesSettings { enabled: true });
    }

    /// Real (non-mocked) integration test of the stdin/stdout protocol added
    /// to `whisper_worker.py --live`, mirroring `stt.rs`'s
    /// `transcribes_real_fixture_and_detects_language`. Bypasses
    /// WASAPI/the resampler entirely: decodes the checked-in fixture to raw
    /// 16kHz mono f32le PCM via a one-off ffmpeg call (what the real
    /// resampler subprocess would hand the daemon), feeds it as
    /// length-prefixed windows over stdin, and asserts real caption text
    /// comes back over stdout for each window.
    ///
    /// Deliberately does not assert on exact fixture wording: independent,
    /// context-free ~2.5s windows (this module's whole design, see
    /// `LIVE_WINDOW_BYTES`'s doc comment) can and do cut words at window
    /// boundaries, so a specific word can land right on a cut and come out
    /// garbled or missing depending on chunk alignment -- that's an
    /// inherent tradeoff of fixed-window re-transcription, not a bug in
    /// this protocol. Transcription *accuracy* on uninterrupted audio is
    /// already covered by `stt.rs`'s batch-mode test; this test is only
    /// responsible for proving the new framing/daemon-loop plumbing works.
    #[tokio::test]
    async fn live_daemon_transcribes_windows_fed_over_stdin() {
        use std::path::Path;

        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("sample_en.wav");
        assert!(fixture.exists(), "fixture missing at {fixture:?}");

        let decode = Command::new("ffmpeg")
            .arg("-y")
            .arg("-i")
            .arg(&fixture)
            .arg("-f")
            .arg("f32le")
            .arg("-ar")
            .arg("16000")
            .arg("-ac")
            .arg("1")
            .arg("pipe:1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .expect("ffmpeg decode should run");
        assert!(decode.status.success(), "ffmpeg decode of fixture failed");
        let pcm = decode.stdout;
        assert!(
            pcm.len() >= LIVE_WINDOW_BYTES,
            "fixture too short to produce even one window"
        );

        let mut daemon = build_live_worker_command(Some("en"), "cpu")
            .spawn()
            .expect("failed to spawn live whisper worker");
        let mut stdin = daemon.stdin.take().expect("daemon stdin was piped");
        let stdout = daemon.stdout.take().expect("daemon stdout was piped");
        if let Some(stderr) = daemon.stderr.take() {
            ffmpeg::spawn_stderr_drain(stderr, "test-live-whisper".to_string());
        }

        let expected_windows = pcm.len() / LIVE_WINDOW_BYTES;
        let write_task = tokio::spawn(async move {
            for chunk in pcm.chunks(LIVE_WINDOW_BYTES) {
                if chunk.len() < LIVE_WINDOW_BYTES {
                    break; // trailing partial window: discarded, matching production EOF behavior
                }
                let len = chunk.len() as u32;
                stdin.write_all(&len.to_le_bytes()).await.unwrap();
                stdin.write_all(chunk).await.unwrap();
            }
            // `stdin` drops here, sending EOF to the daemon.
        });

        let mut lines = BufReader::new(stdout).lines();
        let mut captions = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(LiveWorkerMessage::Caption { text }) =
                serde_json::from_str::<LiveWorkerMessage>(&line)
            {
                if !text.is_empty() {
                    captions.push(text);
                }
            }
        }

        write_task.await.expect("write task should not panic");
        let _ = daemon.wait().await;

        assert!(
            captions.len() >= expected_windows.saturating_sub(1),
            "expected roughly one caption per {expected_windows} full windows, got: {captions:?}"
        );
        assert!(
            captions.iter().all(|c| c.split_whitespace().count() >= 1),
            "each non-empty caption should contain real transcribed words, got: {captions:?}"
        );
    }
}
