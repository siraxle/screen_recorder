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

/// BtbN/FFmpeg-Builds, a static (non-shared) GPL Windows x86_64 build,
/// pinned to one dated autobuild release rather than the rolling `latest`
/// tag. BtbN doesn't publish per-file checksums, so this hash was computed
/// by downloading the asset directly from the pinned URL below. See
/// https://github.com/BtbN/FFmpeg-Builds/releases/tag/autobuild-2026-08-09-13-03
pub const FFMPEG_URL: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-08-09-13-03/ffmpeg-n8.1.2-34-g9b6c8969e0-win64-gpl-8.1.zip";
pub const FFMPEG_SHA256: &str = "dee1b458ad8fa8556d0cc7287a1ec66664d0c2d6a1098d19cf0c465d9d8430bf";
pub const FFMPEG_APPROX_DOWNLOAD_BYTES: u64 = 167_989_881;

/// Rough installed-size estimates (download + unpacked pip packages) used
/// only for the UI's "here's what will be downloaded" disclosure and the
/// disk-space pre-check's safety margin. Deliberately conservative
/// (rounded up) rather than exact -- pip's resolved package set can vary
/// slightly across faster-whisper/onnx-asr releases.
pub const PIP_COMMON_APPROX_INSTALLED_BYTES: u64 = 300 * 1024 * 1024;
pub const PIP_GPU_APPROX_INSTALLED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
