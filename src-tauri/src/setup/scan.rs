//! Detects a Python interpreter or FFmpeg build already installed elsewhere
//! on the machine (PATH plus a handful of common install locations), so
//! `DependencyPanel`'s "Scan my PC" button can offer adopting it instead of
//! downloading a fresh copy. Windows-only, like the rest of `setup` -- the
//! bundled installs this is an alternative to are Windows builds too (see
//! `versions.rs`).

use std::path::PathBuf;

use serde::Serialize;

use super::{paths, DependencyId};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedCandidate {
    pub id: DependencyId,
    /// The folder to hand to `use_dependency_path` -- for `Python`, the
    /// interpreter's own directory (or its venv `Scripts` parent); for
    /// `Ffmpeg`, the directory containing `ffmpeg.exe`/`ffprobe.exe`.
    pub path: String,
    pub detail: String,
}

/// Returns at most one candidate per applicable dependency (the first usable
/// one found) -- this is a convenience shortcut for the common case, not an
/// exhaustive inventory; a user with an unusual install location always has
/// the "browse for folder" fallback.
pub fn scan_all() -> Vec<ScannedCandidate> {
    let mut found = Vec::new();
    found.extend(scan_python());
    found.extend(scan_ffmpeg());
    found
}

fn scan_python() -> Option<ScannedCandidate> {
    for candidate in python_search_dirs() {
        if let Some((_, version)) = paths::probe_python_dir(&candidate) {
            return Some(ScannedCandidate {
                id: DependencyId::Python,
                path: candidate.to_string_lossy().to_string(),
                detail: format!("{version} at {}", candidate.display()),
            });
        }
    }
    None
}

fn scan_ffmpeg() -> Option<ScannedCandidate> {
    for candidate in ffmpeg_search_dirs() {
        if let Some((dir, version)) = paths::probe_ffmpeg_dir(&candidate) {
            return Some(ScannedCandidate {
                id: DependencyId::Ffmpeg,
                path: dir.to_string_lossy().to_string(),
                detail: format!("{version} at {}", dir.display()),
            });
        }
    }
    None
}

/// Full paths reported by `where <exe>`, one per line, deduplicated by the
/// OS's PATH search order. Windows-only; see the module doc.
#[cfg(target_os = "windows")]
fn where_all(exe_name: &str) -> Vec<PathBuf> {
    crate::procutil::std_command("where")
        .arg(exe_name)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| PathBuf::from(l.trim()))
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(not(target_os = "windows"))]
fn where_all(_exe_name: &str) -> Vec<PathBuf> {
    Vec::new()
}

fn python_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        where_all("python.exe").into_iter().filter_map(|exe| exe.parent().map(|p| p.to_path_buf())).collect();

    // The official python.org / Microsoft Store installer's default
    // per-user location: `%LOCALAPPDATA%\Programs\Python\Python3xx\`.
    if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
        let programs = PathBuf::from(local_appdata).join("Programs").join("Python");
        if let Ok(entries) = std::fs::read_dir(&programs) {
            dirs.extend(entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        }
    }

    // The official installer's default all-users location: `C:\Python3xx\`.
    if let Ok(entries) = std::fs::read_dir("C:\\") {
        dirs.extend(
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("Python"))),
        );
    }

    dirs
}

fn ffmpeg_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        where_all("ffmpeg.exe").into_iter().filter_map(|exe| exe.parent().map(|p| p.to_path_buf())).collect();

    for base in ["C:\\ffmpeg", "C:\\Program Files\\ffmpeg", "C:\\Program Files (x86)\\ffmpeg"] {
        dirs.push(PathBuf::from(base));
        dirs.push(PathBuf::from(base).join("bin"));
    }

    dirs
}
