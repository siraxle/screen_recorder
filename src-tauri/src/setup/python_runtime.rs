//! Installs the bundled Python interpreter + venv (`DependencyId::Python`)
//! and, separately, the optional GPU pip extras (`DependencyId::GpuPackages`)
//! into it.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::{archive, download, paths, versions, StageProgressFn};

/// Where `requirements-common.txt`/`requirements-gpu.txt` live -- shared
/// with `pipeline::stt::python_dir` via `paths::bundled_python_scripts_dir`
/// (see its doc comment for why this can't just be `CARGO_MANIFEST_DIR`).
fn repo_python_dir() -> PathBuf {
    paths::bundled_python_scripts_dir()
}

/// Extracts the pinned interpreter, creates a venv, and installs
/// `requirements-common.txt` into it -- all inside a fresh tmp dir,
/// atomically renamed into `runtime/python/` only once every step
/// succeeds. A failure at any point leaves the previous `runtime/python/`
/// (missing, or an older successful install) completely untouched.
pub async fn install_interpreter_and_common(app_data_dir: &Path, on_progress: StageProgressFn) -> Result<()> {
    let runtime_dir = paths::runtime_dir(app_data_dir);
    tokio::fs::create_dir_all(&runtime_dir).await?;
    download::check_free_space(
        &runtime_dir,
        versions::PYTHON_APPROX_DOWNLOAD_BYTES + versions::PIP_COMMON_APPROX_INSTALLED_BYTES,
    )?;

    let tmp = paths::tmp_component_dir(app_data_dir, "python");
    let result = try_install_into(&tmp, &on_progress).await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        return result;
    }

    let final_dir = paths::python_dir(app_data_dir);
    if final_dir.exists() {
        tokio::fs::remove_dir_all(&final_dir).await?;
    }
    tokio::fs::rename(&tmp, &final_dir)
        .await
        .with_context(|| format!("failed to move {tmp:?} into place at {final_dir:?}"))?;

    // `venv` recorded the *tmp* interpreter path (e.g. `home = ...`) into
    // pyvenv.cfg when the venv was created above, since the interpreter
    // still lived under `tmp` at that point. Now that the rename moved
    // both interpreter/ and venv/ together into final_dir, that recorded
    // path is stale -- the venv's python.exe launcher reads it on every
    // startup to find its base interpreter, so leaving it unpatched makes
    // every future launch fail with "No Python at '<stale tmp path>'".
    fix_pyvenv_cfg_home(&final_dir, &tmp.join("interpreter"), &final_dir.join("interpreter")).await?;

    on_progress("done", 1.0);
    Ok(())
}

/// Rewrites any occurrence of `old_interpreter_dir`'s path string in
/// `final_dir/venv/pyvenv.cfg` (the `home`/`executable`/`command` keys) to
/// `new_interpreter_dir`, after the tmp-to-final rename invalidated them.
async fn fix_pyvenv_cfg_home(final_dir: &Path, old_interpreter_dir: &Path, new_interpreter_dir: &Path) -> Result<()> {
    let cfg_path = final_dir.join("venv").join("pyvenv.cfg");
    let contents = tokio::fs::read_to_string(&cfg_path)
        .await
        .with_context(|| format!("failed to read {cfg_path:?}"))?;
    let patched = contents.replace(
        old_interpreter_dir.to_string_lossy().as_ref(),
        new_interpreter_dir.to_string_lossy().as_ref(),
    );
    tokio::fs::write(&cfg_path, patched)
        .await
        .with_context(|| format!("failed to write {cfg_path:?}"))?;
    Ok(())
}

async fn try_install_into(tmp: &Path, on_progress: &StageProgressFn) -> Result<()> {
    tokio::fs::create_dir_all(tmp).await?;

    let archive_path = tmp.join("python.tar.gz");
    let client = reqwest::Client::new();
    {
        let progress = on_progress.clone();
        download::download_with_progress(&client, versions::PYTHON_URL, &archive_path, move |p| {
            progress("downloading", p)
        })
        .await?;
    }
    download::verify_sha256(&archive_path, versions::PYTHON_SHA256)?;

    let extracted = tmp.join("extracted");
    {
        let progress = on_progress.clone();
        let archive_for_task = archive_path.clone();
        let extracted_for_task = extracted.clone();
        tokio::task::spawn_blocking(move || {
            archive::extract_tar_gz(&archive_for_task, &extracted_for_task, move |p| {
                progress("extracting", p)
            })
        })
        .await
        .context("python extraction task panicked")??;
    }

    // The tarball's sole top-level dir is named "python/"; rename it to a
    // fixed name so `paths::python_interpreter_dir` doesn't need to know
    // that detail.
    let extracted_python = archive::single_subdir(&extracted)?;
    let interpreter_dir = tmp.join("interpreter");
    tokio::fs::rename(&extracted_python, &interpreter_dir).await?;
    let _ = tokio::fs::remove_dir_all(&extracted).await;
    let _ = tokio::fs::remove_file(&archive_path).await;

    let interpreter_python = interpreter_dir.join("python.exe");
    let venv_dir = tmp.join("venv");
    create_venv(&interpreter_python, &venv_dir).await?;
    on_progress("installing", 0.05);

    let requirements = repo_python_dir().join("requirements-common.txt");
    let venv_python = venv_dir.join("Scripts").join("python.exe");
    run_pip_install(&venv_python, &requirements, on_progress).await?;

    paths::write_marker(&tmp.join(".installed-common"))?;
    Ok(())
}

/// Installs `requirements-gpu.txt` into the *already-final* venv (unlike
/// the interpreter+common install, this doesn't use a tmp-then-rename dance
/// -- `pip install` into an existing venv is idempotent, so a failed
/// partial GPU install just leaves the GPU marker unwritten and a retry is
/// simply running the same pip install again).
pub async fn install_gpu_packages(app_data_dir: &Path, on_progress: StageProgressFn) -> Result<()> {
    let venv_dir = paths::python_venv_dir(app_data_dir);
    if !venv_dir.is_dir() {
        return Err(anyhow!("{venv_dir:?} doesn't exist -- install Python before GPU packages"));
    }
    download::check_free_space(&venv_dir, versions::PIP_GPU_APPROX_INSTALLED_BYTES)?;

    let venv_python = paths::venv_python_exe(app_data_dir);
    let requirements = repo_python_dir().join("requirements-gpu.txt");
    run_pip_install(&venv_python, &requirements, &on_progress).await?;

    paths::write_marker(&paths::python_gpu_marker(app_data_dir))?;
    on_progress("done", 1.0);
    Ok(())
}

async fn create_venv(interpreter_python: &Path, venv_dir: &Path) -> Result<()> {
    let status = Command::new(interpreter_python)
        .arg("-m")
        .arg("venv")
        .arg(venv_dir)
        .status()
        .await
        .with_context(|| format!("failed to spawn `{interpreter_python:?} -m venv {venv_dir:?}`"))?;
    if !status.success() {
        return Err(anyhow!("`python -m venv` exited with status {status}"));
    }
    Ok(())
}

/// Streams `pip install -r <requirements>`'s stdout, reporting progress as
/// `packages_seen / requirements_line_count` -- pip gives no exact
/// byte-level total, so this is deliberately a coarse approximation, not a
/// precise percentage.
async fn run_pip_install(venv_python: &Path, requirements: &Path, on_progress: &StageProgressFn) -> Result<()> {
    let total = count_requirements(requirements).max(1);
    let mut child = Command::new(venv_python)
        .arg("-m")
        .arg("pip")
        .arg("install")
        .arg("--disable-pip-version-check")
        .arg("-r")
        .arg(requirements)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn pip install -r {requirements:?}"))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let progress = on_progress.clone();
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut seen = 0usize;
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "pip", "{line}");
            if parse_pip_progress_line(&line).is_some() {
                seen += 1;
                progress("installing", (seen as f64 / total as f64).min(1.0));
            }
        }
    });
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "pip", "{line}");
        }
    });

    let status = child.wait().await.context("pip install wait failed")?;
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    if !status.success() {
        return Err(anyhow!("pip install -r {requirements:?} exited with status {status}"));
    }
    on_progress("installing", 1.0);
    Ok(())
}

/// Pulls the package name out of a `Collecting <pkg>` line, pip's signal
/// that it's starting to resolve a new requirement. Split out from the
/// subprocess-streaming code so it's unit-testable without spawning pip.
fn parse_pip_progress_line(line: &str) -> Option<&str> {
    line.strip_prefix("Collecting ").map(str::trim)
}

fn count_requirements(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| {
            s.lines()
                .filter(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.starts_with('#')
                })
                .count()
        })
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pip_progress_line_extracts_package_name() {
        assert_eq!(parse_pip_progress_line("Collecting faster-whisper>=1.2"), Some("faster-whisper>=1.2"));
        assert_eq!(parse_pip_progress_line("  Downloading faster_whisper-1.2-py3-none-any.whl"), None);
        assert_eq!(parse_pip_progress_line("Installing collected packages: faster-whisper"), None);
    }

    #[test]
    fn count_requirements_skips_blank_lines_and_comments() {
        let path = std::env::temp_dir().join(format!("reqs-test-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, "# comment\n\nfaster-whisper>=1.2\nonnx-asr[hub]>=0.9\n").unwrap();
        assert_eq!(count_requirements(&path), 2);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn requirements_common_and_gpu_files_split_correctly() {
        let common = repo_python_dir().join("requirements-common.txt");
        let gpu = repo_python_dir().join("requirements-gpu.txt");
        let common_text = std::fs::read_to_string(&common).unwrap();
        let gpu_text = std::fs::read_to_string(&gpu).unwrap();

        assert!(!common_text.contains("nvidia-cublas"));
        assert!(!common_text.contains("nvidia-cudnn"));
        assert!(!common_text.contains("[gpu"));
        assert!(common_text.contains("faster-whisper"));

        assert!(gpu_text.contains("nvidia-cublas-cu12"));
        assert!(gpu_text.contains("nvidia-cudnn-cu12"));
        assert!(gpu_text.contains("[gpu"));
    }
}
