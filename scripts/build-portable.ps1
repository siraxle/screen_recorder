<#
.SYNOPSIS
    Assembles a portable (no-installer) distribution of Screen Recorder.

.DESCRIPTION
    Unlike `npm run tauri build` (which produces an NSIS/MSI installer and
    writes app data under %APPDATA%), this script builds the raw release exe
    and lays it out in a self-contained folder next to a `data\` directory,
    with a `portable.marker` file that tells the app (see `src-tauri\src\
    portable.rs`) to keep its database, recordings, and runtime\ dependency
    tree (Python venv, ffmpeg) inside that folder instead of the OS app-data
    location. The result can be copied to another machine or a USB stick and
    run with no installation step and nothing left behind in the registry.

    Bundled in this build: the compiled app, its frontend, and a static
    FFmpeg build (pinned to the same release `src-tauri\src\setup\versions.rs`
    downloads on first run for the installed build). NOT bundled: the Python
    interpreter + faster-whisper/onnx-asr venv -- that stays an on-demand
    download from inside the app, same as today, since it is far larger than
    everything else combined (~300MB CPU / multi-GB with GPU extras).

.PARAMETER OutDir
    Where to write the portable folder (and the zip, unless -NoZip is
    passed). Defaults to `dist-portable` at the repo root.

.PARAMETER NoZip
    Skip zipping the assembled folder.
#>
param(
    [string]$OutDir = "dist-portable",
    [switch]$NoZip
)

$ErrorActionPreference = "Stop"

# Must match `src-tauri\src\setup\versions.rs`'s FFMPEG_URL/FFMPEG_SHA256 --
# this is the same pinned static build the installed app downloads on first
# run; re-pinning one without the other means the portable build and the
# installed build ship different ffmpeg versions.
$FfmpegUrl = "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-08-23-13-03/ffmpeg-n8.1.2-44-g7c533d0f86-win64-gpl-8.1.zip"
$FfmpegSha256 = "40d98aef3e8d48665c4dbbdd0093d6e50c61d71a3a48067e9d3edd9fb3a1f3ca"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$SrcTauri = Join-Path $RepoRoot "src-tauri"

Write-Host "==> Building frontend (npm run build)"
Push-Location $RepoRoot
try {
    npm run build
    if ($LASTEXITCODE -ne 0) { throw "npm run build failed with exit code $LASTEXITCODE" }
} finally {
    Pop-Location
}

Write-Host "==> Building release exe (cargo build --release)"
Push-Location $SrcTauri
try {
    # `tauri/custom-protocol` is what `tauri build`/`tauri dev` toggle under
    # the hood to pick embedded-assets-via-custom-protocol vs. loading
    # `devUrl` (http://localhost:1420) at runtime -- our Cargo.toml has no
    # dependency on tauri-cli to set it for us, so a plain `cargo build`
    # here would silently produce a "dev" binary that tries to hit the Vite
    # dev server and fails with ERR_CONNECTION_REFUSED once it's not running.
    cargo build --release --features tauri/custom-protocol
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed with exit code $LASTEXITCODE" }
} finally {
    Pop-Location
}

$ExePath = Join-Path $SrcTauri "target\release\screen-recorder.exe"
if (-not (Test-Path $ExePath)) {
    throw "expected release exe at $ExePath -- check the [package] name in src-tauri\Cargo.toml if it was renamed"
}

$PortableRoot = Join-Path (Join-Path $RepoRoot $OutDir) "ScreenRecorder-portable"
if (Test-Path $PortableRoot) {
    Remove-Item -Recurse -Force $PortableRoot
}
New-Item -ItemType Directory -Force $PortableRoot | Out-Null

Write-Host "==> Copying exe"
Copy-Item $ExePath (Join-Path $PortableRoot "ScreenRecorder.exe")

Write-Host "==> Copying bundled resources (HandBrakeCLI)"
# Flat, directly beside the exe -- NOT under a `resources\` subfolder.
# `compress::handbrake_cli_path` looks it up via Tauri's `resource_dir()`,
# which on Windows always resolves to the exe's own directory (see
# `tauri-utils::platform::resource_dir_from`), matching how
# `bundle.resources` in tauri.conf.json maps it to the flat target
# "HandBrakeCLI.exe" for the installed build. Nesting it here would make
# "Compress for web" silently fail to find HandBrakeCLI.exe in the portable
# build.
Copy-Item (Join-Path $SrcTauri "resources\HandBrakeCLI.exe") $PortableRoot
Copy-Item (Join-Path $SrcTauri "resources\HandBrakeCLI-LICENSE.txt") $PortableRoot

# Worker scripts + pip requirements files, mirroring the "../python/..." ->
# "python/..." mapping in tauri.conf.json's bundle.resources so the
# installed and portable builds resolve them the same way at runtime (see
# `setup::paths::bundled_python_scripts_dir`) -- NOT bundling the interpreter
# itself, which stays an on-demand download (see the note at the bottom of
# this script). Without this, `paths::bundled_python_scripts_dir()`'s
# CARGO_MANIFEST_DIR dev-fallback kicks in, which bakes in this build
# machine's absolute source path and breaks pip install on any other PC.
Write-Host "==> Copying bundled resources (Python worker scripts)"
$PythonDest = Join-Path $PortableRoot "python"
New-Item -ItemType Directory -Force $PythonDest | Out-Null
Copy-Item (Join-Path $RepoRoot "python\whisper_worker.py") $PythonDest
Copy-Item (Join-Path $RepoRoot "python\parakeet_worker.py") $PythonDest
Copy-Item (Join-Path $RepoRoot "python\requirements-common.txt") $PythonDest
Copy-Item (Join-Path $RepoRoot "python\requirements-gpu.txt") $PythonDest

Write-Host "==> Downloading + verifying pinned FFmpeg build"
$FfmpegBinDest = Join-Path $PortableRoot "data\runtime\ffmpeg\bin"
New-Item -ItemType Directory -Force $FfmpegBinDest | Out-Null

$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("screen-recorder-portable-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force $TmpDir | Out-Null
try {
    $ZipPath = Join-Path $TmpDir "ffmpeg.zip"
    Invoke-WebRequest -Uri $FfmpegUrl -OutFile $ZipPath

    $ActualHash = (Get-FileHash -Path $ZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($ActualHash -ne $FfmpegSha256) {
        throw "FFmpeg download hash mismatch`n  expected: $FfmpegSha256`n  actual:   $ActualHash"
    }

    $ExtractDir = Join-Path $TmpDir "extracted"
    Expand-Archive -Path $ZipPath -DestinationPath $ExtractDir

    # The zip's sole top-level dir is named after the exact build (a
    # git-hash suffix that changes every release) -- located dynamically,
    # mirroring `setup::archive::single_subdir` in the Rust installer.
    $BuildDir = Get-ChildItem -Path $ExtractDir -Directory | Select-Object -First 1
    if (-not $BuildDir) { throw "no top-level directory found in extracted FFmpeg archive" }

    Copy-Item (Join-Path $BuildDir.FullName "bin\ffmpeg.exe") $FfmpegBinDest
    Copy-Item (Join-Path $BuildDir.FullName "bin\ffprobe.exe") $FfmpegBinDest
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}

# Marker consumed by `setup::paths::is_installed`/`setup::check_all` --
# without it the app would think ffmpeg still needs installing despite the
# binaries already being on disk.
New-Item -ItemType File -Force (Join-Path $PortableRoot "data\runtime\ffmpeg\.installed") | Out-Null

Write-Host "==> Writing portable.marker"
New-Item -ItemType File -Force (Join-Path $PortableRoot "portable.marker") | Out-Null

if (-not $NoZip) {
    Write-Host "==> Zipping"
    $ZipDest = Join-Path (Join-Path $RepoRoot $OutDir) "ScreenRecorder-portable.zip"
    if (Test-Path $ZipDest) { Remove-Item -Force $ZipDest }
    Compress-Archive -Path $PortableRoot -DestinationPath $ZipDest
    Write-Host "==> Done: $ZipDest"
} else {
    Write-Host "==> Done: $PortableRoot"
}

Write-Host ""
Write-Host "Note: Python/faster-whisper is NOT bundled -- the app will prompt to" -ForegroundColor Yellow
Write-Host "download it into data\runtime\python\ on first run (or point it at an" -ForegroundColor Yellow
Write-Host "existing system Python via Settings > Dependencies)." -ForegroundColor Yellow
