//! Runtime dependency layout under `<app_data_dir>/runtime/`, marker files
//! that mark a component as fully (not partially) installed, and a
//! process-wide cache of the resolved interpreter/binary paths so
//! `pipeline::stt` and `capture::ffmpeg` don't need an `AppHandle` threaded
//! through every command-builder call.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub fn runtime_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("runtime")
}

pub fn python_dir(app_data_dir: &Path) -> PathBuf {
    runtime_dir(app_data_dir).join("python")
}

pub fn python_venv_dir(app_data_dir: &Path) -> PathBuf {
    python_dir(app_data_dir).join("venv")
}

pub fn venv_python_exe(app_data_dir: &Path) -> PathBuf {
    python_venv_dir(app_data_dir).join("Scripts").join("python.exe")
}

/// Directory containing the bundled Python worker scripts
/// (`whisper_worker.py`, `parakeet_worker.py`) and pip requirements files
/// (`requirements-common.txt`, `requirements-gpu.txt`). Not to be confused
/// with `python_dir` above, which is the *installed runtime* (interpreter +
/// venv) under `<app_data_dir>/runtime/python` -- this is the read-only
/// source material that gets pip-installed into that runtime.
///
/// Resolved relative to the running exe's own location, never via
/// `CARGO_MANIFEST_DIR` (a compile-time constant baked into the binary that
/// names wherever it happened to be *built*, not wherever it's *running*).
/// Using that constant at runtime used to work by accident in dev
/// (`cargo tauri dev`/`cargo test` never leave the dev machine) but broke
/// every distributed build: an installed or portable copy shipped to a
/// different PC still carried the original dev machine's absolute path
/// baked in, so pip's `-r <path>` pointed at a directory that doesn't
/// exist there and failed immediately. See `bundle.resources` in
/// `tauri.conf.json` (installed build) and `scripts/build-portable.ps1`
/// (portable build), both of which place a copy of `python/` at
/// `<exe_dir>/python`.
pub fn bundled_python_scripts_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let bundled = exe_dir.join("python");
            if bundled.join("whisper_worker.py").is_file() {
                return bundled;
            }
        }
    }
    // Dev fallback: no bundled copy sits next to a `cargo build`/`cargo
    // test` profile exe, so read straight from the source tree.
    // CARGO_MANIFEST_DIR is `<repo>/src-tauri` at compile time.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("python")
}

pub fn ffmpeg_dir(app_data_dir: &Path) -> PathBuf {
    runtime_dir(app_data_dir).join("ffmpeg")
}

pub fn ffmpeg_exe(app_data_dir: &Path) -> PathBuf {
    ffmpeg_dir(app_data_dir).join("bin").join("ffmpeg.exe")
}

pub fn ffprobe_exe(app_data_dir: &Path) -> PathBuf {
    ffmpeg_dir(app_data_dir).join("bin").join("ffprobe.exe")
}

/// Written inside the python component's tmp dir before the final rename,
/// so it only ever exists once `python -m venv` + the common pip install
/// both completed successfully.
pub fn python_common_marker(app_data_dir: &Path) -> PathBuf {
    python_dir(app_data_dir).join(".installed-common")
}

/// Written in-place into the (already final) venv after the GPU pip extras
/// finish installing -- unlike the other markers, this one is not part of
/// an atomic rename, since installing into an existing venv is idempotent
/// (see `setup::install`'s GPU-packages branch).
pub fn python_gpu_marker(app_data_dir: &Path) -> PathBuf {
    python_venv_dir(app_data_dir).join(".installed-gpu")
}

pub fn ffmpeg_marker(app_data_dir: &Path) -> PathBuf {
    ffmpeg_dir(app_data_dir).join(".installed")
}

pub fn is_installed(marker: &Path) -> bool {
    marker.is_file()
}

pub fn write_marker(marker: &Path) -> std::io::Result<()> {
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(marker, b"ok")
}

/// A fresh scratch directory for one install attempt, never reused across
/// attempts (see `setup::install`'s tmp-then-rename atomicity). Callers are
/// responsible for best-effort cleanup on failure; a leftover `.tmp-*` dir
/// is harmless since `check_all` never looks at it.
pub fn tmp_component_dir(app_data_dir: &Path, component: &str) -> PathBuf {
    runtime_dir(app_data_dir).join(format!(".tmp-{component}-{}", uuid::Uuid::new_v4()))
}

// Process-wide resolved paths, first set during `lib.rs`'s `.setup()` hook
// right after `app_data_dir` is known (see `super::resolve_startup_paths`),
// and re-set at runtime whenever the user adopts a system/custom dependency
// path (see `super::apply_override`) -- a `Mutex` rather than the `OnceLock`
// this used to be, since that override can happen more than once per run.
// Default to the bare PATH-relative literal when unset, so dev/test flows
// that assume a system python/ffmpeg on PATH (e.g. `compress.rs`'s and
// `stt.rs`'s existing tests) keep working unchanged.
static PYTHON_EXE: Mutex<Option<PathBuf>> = Mutex::new(None);
static FFMPEG_EXE: Mutex<Option<PathBuf>> = Mutex::new(None);
static FFPROBE_EXE: Mutex<Option<PathBuf>> = Mutex::new(None);
static GPU_PACKAGES_READY: AtomicBool = AtomicBool::new(false);

pub fn set_resolved_python(path: PathBuf) {
    *PYTHON_EXE.lock().unwrap() = Some(path);
}

pub fn set_resolved_ffmpeg(path: PathBuf) {
    *FFMPEG_EXE.lock().unwrap() = Some(path);
}

pub fn set_resolved_ffprobe(path: PathBuf) {
    *FFPROBE_EXE.lock().unwrap() = Some(path);
}

/// Refreshed after every `check_all`/`install` call -- this is what STT's
/// `Auto` device resolution actually reads (see `pipeline::stt::resolve_device`):
/// a GPU being physically present is not enough to pick `cuda` if the large
/// optional GPU pip extras were never installed.
pub fn set_gpu_packages_ready(ready: bool) {
    GPU_PACKAGES_READY.store(ready, Ordering::Relaxed);
}

pub fn gpu_packages_ready() -> bool {
    GPU_PACKAGES_READY.load(Ordering::Relaxed)
}

pub fn python_program() -> PathBuf {
    PYTHON_EXE.lock().unwrap().clone().unwrap_or_else(|| PathBuf::from("python"))
}

pub fn ffmpeg_program() -> PathBuf {
    FFMPEG_EXE.lock().unwrap().clone().unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

pub fn ffprobe_program() -> PathBuf {
    FFPROBE_EXE.lock().unwrap().clone().unwrap_or_else(|| PathBuf::from("ffprobe"))
}

/// Spawns `<candidate> --version` and returns the parsed banner (e.g.
/// `"Python 3.11.4"`) if `candidate` is a working Python interpreter.
/// Shared by `scan` (auto-detection) and the "browse to a folder" flow so
/// both go through the same validation before a path is ever adopted.
pub fn probe_python_version(candidate: &Path) -> Option<String> {
    if !candidate.is_file() {
        return None;
    }
    let output = crate::procutil::std_command(candidate).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    // Python prints its version banner to stdout on 3.4+; this app already
    // requires a far newer interpreter for `faster-whisper`/`onnx-asr`, so
    // stdout-only is sufficient.
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Checks `dir` (and, since a bare interpreter directory and a venv both get
/// browsed to in practice, `dir/Scripts`) for a working `python.exe`.
/// Returns the exe path plus its parsed version banner on success.
pub fn probe_python_dir(dir: &Path) -> Option<(PathBuf, String)> {
    for candidate in [dir.join("python.exe"), dir.join("Scripts").join("python.exe")] {
        if let Some(version) = probe_python_version(&candidate) {
            return Some((candidate, version));
        }
    }
    None
}

/// Same idea as `probe_python_version`, for an `ffmpeg.exe` candidate.
pub fn probe_ffmpeg_version(candidate: &Path) -> Option<String> {
    if !candidate.is_file() {
        return None;
    }
    let output = crate::procutil::std_command(candidate).arg("-version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    // ffmpeg's `-version` banner's first line is e.g. "ffmpeg version n8.1.2 ...".
    String::from_utf8_lossy(&output.stdout).lines().next().map(|l| l.trim().to_string())
}

/// Checks `dir` (and `dir/bin`, matching the bundled build's own layout --
/// see `ffmpeg_dir`/`ffmpeg_exe` above) for both `ffmpeg.exe` and
/// `ffprobe.exe`. Returns the directory that actually contains them (not
/// necessarily `dir` itself) plus ffmpeg's parsed version banner.
pub fn probe_ffmpeg_dir(dir: &Path) -> Option<(PathBuf, String)> {
    for candidate in [dir.to_path_buf(), dir.join("bin")] {
        let ffmpeg_exe = candidate.join("ffmpeg.exe");
        let ffprobe_exe = candidate.join("ffprobe.exe");
        if ffprobe_exe.is_file() {
            if let Some(version) = probe_ffmpeg_version(&ffmpeg_exe) {
                return Some((candidate, version));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trips() {
        let dir = std::env::temp_dir().join(format!("setup-paths-test-{}", uuid::Uuid::new_v4()));
        let marker = dir.join("sub").join(".installed");
        assert!(!is_installed(&marker));
        write_marker(&marker).unwrap();
        assert!(is_installed(&marker));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn probe_python_dir_rejects_a_folder_with_no_interpreter() {
        let dir = std::env::temp_dir().join(format!("setup-paths-test-py-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(probe_python_dir(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn probe_ffmpeg_dir_rejects_a_folder_with_no_binaries() {
        let dir = std::env::temp_dir().join(format!("setup-paths-test-ff-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(probe_ffmpeg_dir(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tmp_component_dirs_are_unique() {
        let base = Path::new("C:\\fake\\app-data");
        assert_ne!(tmp_component_dir(base, "python"), tmp_component_dir(base, "python"));
    }
}
