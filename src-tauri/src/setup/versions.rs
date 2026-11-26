//! Pinned download sources for the runtime dependencies `setup` installs.
//! Deliberately pinned to specific, immutable release assets (never a
//! rolling "latest" tag) so a checksum stays meaningful and an install
//! today behaves the same as an install next year. Re-pinning is a
//! deliberate, occasional maintenance action, not something `setup` does
//! automatically.

/// astral-sh/python-build-standalone, CPython 3.12 "install_only" build for
/// Windows x86_64 -- a self-contained interpreter (includes `ensurepip`, no
/// separate installer needed) that `python_runtime::install` extracts and
/// then builds a venv from. See https://github.com/astral-sh/python-build-standalone/releases/tag/20260807
pub const PYTHON_URL: &str = "https://github.com/astral-sh/python-build-standalone/releases/download/20260807/cpython-3.12.13+20260807-x86_64-pc-windows-msvc-install_only.tar.gz";
pub const PYTHON_SHA256: &str = "6cf2be701aa7e9470454c9c86285c1bcc1832518d63e39c3e34e9d8ea1cbb99f";
/// Approximate download size in bytes, for the pre-install disclosure UI and
/// the disk-space check's margin -- not required to be exact.
pub const PYTHON_APPROX_DOWNLOAD_BYTES: u64 = 46_113_914;

/// FFmpeg-Builds/FFmpeg-Builds (formerly BtbN/FFmpeg-Builds), a static (non-shared) GPL Windows x86_64 build,
/// pinned to one dated autobuild release rather than the rolling `latest`
/// tag. The hash below matches the asset digest GitHub reports and was
/// re-verified by downloading the asset from the pinned URL. Old autobuild
/// releases get pruned, so this pin must be bumped when it starts 404ing. See
/// https://github.com/FFmpeg-Builds/FFmpeg-Builds/releases/tag/autobuild-2026-11-22-12-36
pub const FFMPEG_URL: &str = "https://github.com/FFmpeg-Builds/FFmpeg-Builds/releases/download/autobuild-2026-11-22-12-36/ffmpeg-n8.1.3-39-g660b5aa7db-win64-gpl-8.1.zip";
pub const FFMPEG_SHA256: &str = "b16431997ec2685c1baab7dc214537cdbdd48dabdd6e233d4ab0f1e316de0dc7";
pub const FFMPEG_APPROX_DOWNLOAD_BYTES: u64 = 197_396_631;

/// Rough installed-size estimates (download + unpacked pip packages) used
/// only for the UI's "here's what will be downloaded" disclosure and the
/// disk-space pre-check's safety margin. Deliberately conservative
/// (rounded up) rather than exact -- pip's resolved package set can vary
/// slightly across faster-whisper/onnx-asr releases.
pub const PIP_COMMON_APPROX_INSTALLED_BYTES: u64 = 300 * 1024 * 1024;
pub const PIP_GPU_APPROX_INSTALLED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
