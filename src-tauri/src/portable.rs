//! Portable-distribution support: when the exe ships with a `portable.marker`
//! file next to it (dropped there by `scripts/build-portable.ps1`, never by
//! the installed/NSIS build), all app state -- db, recordings, and the
//! `setup::paths` runtime/ tree (Python venv, ffmpeg) -- lives in `data/`
//! beside the exe instead of the OS app-data location. That keeps a portable
//! copy self-contained: no registry entries, nothing left behind in
//! `%APPDATA%`, safe to run off a USB stick or copy to another machine.
//!
//! Every call site that used to reach for `app.path().app_data_dir()`
//! directly must go through `app_data_dir` here instead, so the portable and
//! installed builds share one code path with no other behavioral branching.

use std::path::PathBuf;

use tauri::{Manager, Runtime};

fn portable_data_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    if exe_dir.join("portable.marker").is_file() {
        Some(exe_dir.join("data"))
    } else {
        None
    }
}

pub fn app_data_dir<R: Runtime, M: Manager<R>>(app: &M) -> tauri::Result<PathBuf> {
    match portable_data_dir() {
        Some(dir) => Ok(dir),
        None => app.path().app_data_dir(),
    }
}
