//! Dependency detection and on-demand install for the runtimes the app
//! needs but doesn't bundle in the installer: a Python interpreter + venv
//! (see `python_runtime`), a static FFmpeg build (see `ffmpeg_runtime`),
//! and -- only on machines with a real NVIDIA GPU -- the large CUDA pip
//! extras. Everything lives under `<app_data_dir>/runtime/`.
//!
//! `check_all` is cheap and read-only (just stats a few marker files) --
//! call it freely from the frontend on mount / whenever the Dependencies
//! tab opens. `install` actually downloads and installs; it's the only
//! function in this module that touches the network or spawns pip.

pub mod archive;
pub mod download;
mod ffmpeg_runtime;
pub mod overrides;
pub mod paths;
mod python_runtime;
pub mod scan;
mod versions;

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

pub use overrides::DependencySource;
pub use scan::ScannedCandidate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyId {
    Python,
    Ffmpeg,
    GpuPackages,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyStatus {
    pub id: DependencyId,
    pub satisfied: bool,
    /// `GpuPackages` only: false on a machine with no NVIDIA GPU, in which
    /// case `satisfied` is meaningless and must never gate the app -- the
    /// CPU fallback already works.
    pub applicable: bool,
    pub detail: String,
    /// Rough total bytes this dependency's install will transfer/write --
    /// for the frontend's pre-download disclosure, not a hard guarantee.
    pub approx_download_bytes: u64,
    /// Where the resolved binary actually comes from -- always `Bundled`
    /// for `GpuPackages` (see module doc). Lets the frontend show "using
    /// your system install at ..." instead of the normal install/reinstall
    /// controls.
    pub source: DependencySource,
    /// Set when `source != Bundled`: the folder the user's system-detected
    /// or manually-browsed-to dependency lives in.
    pub override_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyReport {
    pub gpu_present: bool,
    pub statuses: Vec<DependencyStatus>,
}

impl DependencyReport {
    /// Gate criterion for the frontend's first-run screen -- only Python
    /// and Ffmpeg are hard requirements; GpuPackages never blocks since the
    /// CPU path always works.
    pub fn required_satisfied(&self) -> bool {
        self.statuses
            .iter()
            .filter(|s| s.id != DependencyId::GpuPackages)
            .all(|s| s.satisfied)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallProgress {
    pub dependency: DependencyId,
    pub stage: &'static str, // "downloading" | "extracting" | "installing" | "done"
    pub progress: f64,       // 0.0..=1.0 within the stage
}

/// Shared by `python_runtime`/`ffmpeg_runtime` for per-stage progress
/// reporting -- an `Arc` (not a generic `Fn + Clone` bound) so it can be
/// cheaply cloned into `tokio::spawn`/`spawn_blocking` closures without
/// pushing `Clone` bounds through every function signature in this module.
pub(crate) type StageProgressFn = Arc<dyn Fn(&'static str, f64) + Send + Sync>;

/// A dependency's override (if any) plus whether it's still valid -- an
/// override whose folder was moved/deleted since it was set is reported back
/// as unsatisfied rather than silently falling back to the bundled install,
/// so the user notices and re-points it rather than getting a confusing
/// "works today, broken tomorrow" failure deeper in the pipeline.
fn resolved_override(
    overrides_map: &std::collections::HashMap<DependencyId, overrides::Override>,
    id: DependencyId,
    probe: impl Fn(&Path) -> Option<(std::path::PathBuf, String)>,
) -> Option<(overrides::Override, String)> {
    let ov = overrides_map.get(&id)?;
    let (_, version) = probe(&ov.path)?;
    Some((ov.clone(), version))
}

pub async fn check_all(app_data_dir: &Path) -> DependencyReport {
    let gpu_present = crate::thermal::gpu_present().await;
    let overrides_map = overrides::load(app_data_dir);

    let python_bundled_ok = paths::is_installed(&paths::python_common_marker(app_data_dir));
    let python_override = resolved_override(&overrides_map, DependencyId::Python, paths::probe_python_dir);
    let python_ok = python_bundled_ok || python_override.is_some();

    let ffmpeg_bundled_ok = paths::is_installed(&paths::ffmpeg_marker(app_data_dir));
    let ffmpeg_override = resolved_override(&overrides_map, DependencyId::Ffmpeg, paths::probe_ffmpeg_dir);
    let ffmpeg_ok = ffmpeg_bundled_ok || ffmpeg_override.is_some();

    // GPU packages are pip extras installed into the *bundled* python venv
    // specifically (see module doc) -- if Python is currently resolving to
    // a system/custom override instead, those extras aren't reachable no
    // matter what the bundled venv itself contains.
    let gpu_bundled_ok = python_bundled_ok && paths::is_installed(&paths::python_gpu_marker(app_data_dir));
    let python_has_override = python_override.is_some();
    let gpu_ok = gpu_bundled_ok && !python_has_override;
    paths::set_gpu_packages_ready(gpu_ok);

    DependencyReport {
        gpu_present,
        statuses: vec![
            DependencyStatus {
                id: DependencyId::Python,
                satisfied: python_ok,
                applicable: true,
                detail: match &python_override {
                    Some((ov, version)) => format!("Using {version} ({})", source_label(ov.source)),
                    None if python_bundled_ok => "Installed".into(),
                    None => "Not installed".into(),
                },
                approx_download_bytes: versions::PYTHON_APPROX_DOWNLOAD_BYTES
                    + versions::PIP_COMMON_APPROX_INSTALLED_BYTES,
                source: python_override.as_ref().map(|(ov, _)| ov.source).unwrap_or(DependencySource::Bundled),
                override_path: python_override.map(|(ov, _)| ov.path.to_string_lossy().to_string()),
            },
            DependencyStatus {
                id: DependencyId::Ffmpeg,
                satisfied: ffmpeg_ok,
                applicable: true,
                detail: match &ffmpeg_override {
                    Some((ov, version)) => format!("Using {version} ({})", source_label(ov.source)),
                    None if ffmpeg_bundled_ok => "Installed".into(),
                    None => "Not installed".into(),
                },
                approx_download_bytes: versions::FFMPEG_APPROX_DOWNLOAD_BYTES,
                source: ffmpeg_override.as_ref().map(|(ov, _)| ov.source).unwrap_or(DependencySource::Bundled),
                override_path: ffmpeg_override.map(|(ov, _)| ov.path.to_string_lossy().to_string()),
            },
            DependencyStatus {
                id: DependencyId::GpuPackages,
                satisfied: gpu_ok,
                applicable: gpu_present,
                detail: if !gpu_present {
                    "No NVIDIA GPU detected".into()
                } else if python_has_override {
                    "Not available while using a custom/system Python".into()
                } else if gpu_ok {
                    "Installed".into()
                } else {
                    "Not installed (optional -- CPU works without it)".into()
                },
                approx_download_bytes: versions::PIP_GPU_APPROX_INSTALLED_BYTES,
                source: DependencySource::Bundled,
                override_path: None,
            },
        ],
    }
}

fn source_label(source: DependencySource) -> &'static str {
    match source {
        DependencySource::Bundled => "bundled",
        DependencySource::System => "detected on your PC",
        DependencySource::Custom => "custom location",
    }
}

/// Resolves the effective python/ffmpeg/ffprobe binaries for this process:
/// a still-valid user override (see `overrides`) wins over the bundled
/// `runtime/` install. Called once from `lib.rs`'s `.setup()` hook, and
/// again by `apply_override`/`clear_override` whenever the choice changes at
/// runtime.
pub fn resolve_startup_paths(app_data_dir: &Path) {
    let overrides_map = overrides::load(app_data_dir);

    match overrides_map.get(&DependencyId::Python).and_then(|ov| paths::probe_python_dir(&ov.path)) {
        Some((exe, _)) => paths::set_resolved_python(exe),
        None => paths::set_resolved_python(paths::venv_python_exe(app_data_dir)),
    }

    match overrides_map.get(&DependencyId::Ffmpeg).and_then(|ov| paths::probe_ffmpeg_dir(&ov.path)) {
        Some((dir, _)) => {
            paths::set_resolved_ffmpeg(dir.join("ffmpeg.exe"));
            paths::set_resolved_ffprobe(dir.join("ffprobe.exe"));
        }
        None => {
            paths::set_resolved_ffmpeg(paths::ffmpeg_exe(app_data_dir));
            paths::set_resolved_ffprobe(paths::ffprobe_exe(app_data_dir));
        }
    }
}

/// Validates `folder` actually contains the dependency it claims to, then
/// persists it as an override and re-resolves the live process-wide path --
/// the effect is immediate, no restart needed. Used both by the "Scan my PC"
/// accept action (`source: System`) and the "browse for folder" flow
/// (`source: Custom`).
pub fn apply_override(
    app_data_dir: &Path,
    id: DependencyId,
    folder: &Path,
    source: DependencySource,
) -> Result<String> {
    let version = match id {
        DependencyId::Python => {
            paths::probe_python_dir(folder).ok_or_else(|| anyhow!("no working python.exe found in {folder:?}"))?.1
        }
        DependencyId::Ffmpeg => {
            paths::probe_ffmpeg_dir(folder)
                .ok_or_else(|| anyhow!("no working ffmpeg.exe/ffprobe.exe found in {folder:?}"))?
                .1
        }
        DependencyId::GpuPackages => {
            return Err(anyhow!("GPU packages are only ever installed into the app's own Python -- pick a folder for Python instead"))
        }
    };

    overrides::set(app_data_dir, id, overrides::Override { path: folder.to_path_buf(), source })?;
    resolve_startup_paths(app_data_dir);
    Ok(version)
}

/// Reverts `id` back to the bundled `runtime/` install (which may itself
/// still need `install`ing -- this only removes the override, same as if it
/// had never been set).
pub fn clear_override(app_data_dir: &Path, id: DependencyId) -> Result<()> {
    overrides::clear(app_data_dir, id)?;
    resolve_startup_paths(app_data_dir);
    Ok(())
}

/// Installs each target in order (so a combined `[Python, GpuPackages]`
/// request works in one call). A failure partway through leaves earlier
/// targets in this batch installed and later ones untouched -- re-running
/// `install` with the still-missing targets is always safe (see
/// `python_runtime`/`ffmpeg_runtime`'s tmp-dir-then-rename atomicity).
pub async fn install(
    app_data_dir: &Path,
    targets: &[DependencyId],
    on_progress: impl Fn(InstallProgress) + Send + Sync + 'static,
) -> Result<()> {
    let on_progress = Arc::new(on_progress);
    for &target in targets {
        let op = on_progress.clone();
        let stage_fn: StageProgressFn = Arc::new(move |stage, progress| {
            op(InstallProgress { dependency: target, stage, progress });
        });
        match target {
            DependencyId::Python => {
                python_runtime::install_interpreter_and_common(app_data_dir, stage_fn).await?
            }
            DependencyId::Ffmpeg => ffmpeg_runtime::install(app_data_dir, stage_fn).await?,
            DependencyId::GpuPackages => {
                if !paths::is_installed(&paths::python_common_marker(app_data_dir)) {
                    return Err(anyhow!("Python must be installed before GPU packages"));
                }
                python_runtime::install_gpu_packages(app_data_dir, stage_fn).await?
            }
        }
    }
    Ok(())
}
