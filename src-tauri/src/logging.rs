//! App-wide logging: every `tracing::info!/warn!/error!` call (recording
//! actions, dependency downloads/installs, command failures, ...) goes both
//! to stderr (for `npm run tauri dev`) and to `<app_data_dir>/logs/app.log`,
//! so a user hitting a problem -- especially during the portable build's
//! on-demand Python/FFmpeg download -- can pull one file and hand it over
//! instead of reproducing the issue live. `init` must run first thing inside
//! `lib.rs`'s `.setup()` hook, once `portable::app_data_dir` is resolved
//! (same directory portable and installed builds already use for the db and
//! recordings, so logs move with them).
//!
//! The Settings window's "Download log" button (see `commands::download_log`)
//! reads the path back via `log_file_path`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

const LOG_FILE_NAME: &str = "app.log";
/// Above this size, the previous run's log is rotated to `app.log.old`
/// (overwriting any earlier `.old`) instead of growing `app.log` forever --
/// this is a single-user desktop app, not a server, so one backup gen is
/// plenty rather than a full rotation scheme.
const ROTATE_AT_BYTES: u64 = 5 * 1024 * 1024;

static LOG_FILE_PATH: OnceLock<PathBuf> = OnceLock::new();
// The non-blocking writer only flushes while this guard is alive -- held for
// the whole process lifetime so buffered lines aren't lost on drop.
static WORKER_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Sets up the global tracing subscriber. Safe to call exactly once; a
/// second call is a bug (would panic on the global-default already-set), so
/// this must stay the very first line of `lib.rs`'s `.setup()` hook.
pub fn init(app_data_dir: &Path) {
    let log_dir = app_data_dir.join("logs");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!("failed to create log directory {log_dir:?}: {e}");
    }

    let log_path = log_dir.join(LOG_FILE_NAME);
    rotate_if_large(&log_path);
    LOG_FILE_PATH.set(log_path).ok();

    let file_appender = tracing_appender::rolling::never(&log_dir, LOG_FILE_NAME);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    WORKER_GUARD.set(guard).ok();

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(non_blocking)
        .with_filter(env_filter());
    let console_layer = tracing_subscriber::fmt::layer().with_filter(env_filter());

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();
}

fn rotate_if_large(log_path: &Path) {
    let Ok(metadata) = std::fs::metadata(log_path) else { return };
    if metadata.len() < ROTATE_AT_BYTES {
        return;
    }
    let backup_path = log_path.with_extension("log.old");
    let _ = std::fs::rename(log_path, backup_path);
}

/// The active log file's path, for the "Download log" command. `None` only
/// if called before `init` runs, which shouldn't happen once the app is up.
pub fn log_file_path() -> Option<PathBuf> {
    LOG_FILE_PATH.get().cloned()
}

/// Logs `e` at error level, then stringifies it -- meant to replace the bare
/// `.map_err(|e| e.to_string())` pattern in `commands.rs` so every error
/// that reaches the frontend is captured in the log file too, not just
/// shown once in a toast and lost.
pub fn log_err<E: std::fmt::Display>(e: E) -> String {
    let msg = e.to_string();
    tracing::error!("{msg}");
    msg
}
