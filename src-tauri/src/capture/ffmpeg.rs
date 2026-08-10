use std::path::Path;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};

use super::config::{CaptureConfig, WebcamCorner, WebcamOverlayConfig, WebcamShape};
use super::system_audio;

/// Reads a child's stderr to completion in the background and logs it via
/// `tracing`. ffmpeg writes continuous progress stats to stderr; without
/// draining it, the OS pipe buffer fills up and the process blocks trying to
/// write, silently deadlocking anything awaiting its exit.
pub fn spawn_stderr_drain(stderr: ChildStderr, label: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => tracing::debug!(target: "ffmpeg", "[{label}] {line}"),
                _ => break,
            }
        }
    })
}

/// Spawns a short-lived ffmpeg command (mux/concat passes), drains its
/// stderr, and waits for it to finish successfully.
pub async fn run_to_completion(mut cmd: Command, label: &str) -> Result<()> {
    let mut child: Child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn ffmpeg ({label})"))?;
    let drain = child
        .stderr
        .take()
        .map(|stderr| spawn_stderr_drain(stderr, label.to_string()));
    let status = child
        .wait()
        .await
        .with_context(|| format!("ffmpeg ({label}) wait failed"))?;
    if let Some(handle) = drain {
        let _ = handle.await;
    }
    if !status.success() {
        return Err(anyhow!("ffmpeg ({label}) exited with status {status}"));
    }
    Ok(())
}

/// Builds the ffmpeg process that records one segment: desktop video via
/// gdigrab, plus the microphone via dshow if enabled. System audio is
/// captured separately (see `system_audio.rs`, WASAPI has no ffmpeg/dshow
/// path on stock Windows) and muxed in afterwards by `build_mux_command`.
///
/// `+frag_keyframe+empty_moov` makes the mp4 playable even if the process is
/// killed instead of stopped gracefully — segment boundaries (pause/resume)
/// must never corrupt already-recorded footage.
#[cfg(target_os = "windows")]
pub fn build_segment_command(config: &CaptureConfig, output_path: &Path) -> Command {
    let mut cmd = Command::new(crate::setup::paths::ffmpeg_program());
    cmd.arg("-y")
        .arg("-f")
        .arg("gdigrab")
        .arg("-framerate")
        .arg(config.framerate.to_string())
        .arg("-i")
        .arg("desktop");

    // Track input indices as optional inputs are added below (desktop is
    // always input 0), so the F16 webcam-overlay filter graph and the
    // (explicit, once -filter_complex is in play) audio -map can reference
    // the right stream regardless of which combination of mic/webcam is
    // enabled this session.
    let mut next_input_index: usize = 1;

    let webcam_index = if config.webcam.enabled {
        // dshow has no "use OS default" device name; callers must resolve a
        // concrete device (see `CaptureController::begin_segment`) before
        // this is invoked, same as the microphone below.
        let device = config
            .webcam
            .device
            .as_deref()
            .expect("webcam.enabled requires a resolved webcam.device");
        cmd.arg("-f")
            .arg("dshow")
            .arg("-framerate")
            .arg(config.framerate.to_string())
            .arg("-i")
            .arg(format!("video={device}"));
        let idx = next_input_index;
        next_input_index += 1;
        Some(idx)
    } else {
        None
    };

    let mic_index = if config.capture_microphone {
        let mic = config
            .microphone_device
            .as_deref()
            .expect("capture_microphone requires a resolved microphone_device");
        cmd.arg("-f")
            .arg("dshow")
            .arg("-i")
            .arg(format!("audio={mic}"));
        Some(next_input_index)
    } else {
        None
    };

    // Without the webcam overlay, stream mapping stays fully implicit
    // (ffmpeg's default "one stream per type" selection), exactly matching
    // this command's shape before F16 -- so a recording with the overlay
    // disabled is byte-for-byte unaffected by this feature's existence.
    if let Some(cam_idx) = webcam_index {
        cmd.arg("-filter_complex")
            .arg(build_webcam_overlay_filter(cam_idx, &config.webcam));
        cmd.arg("-map").arg("[outv]");
        if let Some(mic_idx) = mic_index {
            cmd.arg("-map").arg(format!("{mic_idx}:a"));
        }
    }

    cmd.arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("ultrafast")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-movflags")
        .arg("+frag_keyframe+empty_moov");

    if config.capture_microphone {
        cmd.arg("-c:a").arg("aac").arg("-b:a").arg("160k");
    }

    cmd.arg(output_path);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Builds the F16 webcam-overlay filter graph: crops the webcam feed to a
/// square, scales it to a fixed fraction of the desktop capture's own width
/// (via `scale2ref`, so this never needs to query the desktop's actual
/// resolution), optionally masks it into a circle, then overlays it onto the
/// desktop video in the configured corner with a fixed margin.
#[cfg(target_os = "windows")]
fn build_webcam_overlay_filter(cam_index: usize, webcam: &WebcamOverlayConfig) -> String {
    // Fixed rather than user-configurable -- the user asked for corner and
    // shape to be pickable, not a size slider; both can become settings
    // later if wanted. 0.45 = 2.5x the original 0.18 default.
    const SIZE_FRACTION: &str = "0.45";
    const MARGIN_PX: i32 = 24;

    let mask = match webcam.shape {
        // Hard-edge circular alpha mask (no feathering) -- simple and cheap
        // enough on an already-small (scaled-down) frame.
        WebcamShape::Circle => {
            "[camscaled]format=yuva420p,geq=lum='p(X,Y)':a='if(lte(pow(X-W/2,2)+pow(Y-H/2,2),pow(W/2,2)),255,0)'[camfinal]"
                .to_string()
        }
        WebcamShape::Square => "[camscaled]copy[camfinal]".to_string(),
    };

    let (x, y) = match webcam.corner {
        WebcamCorner::TopLeft => (format!("{MARGIN_PX}"), format!("{MARGIN_PX}")),
        WebcamCorner::TopRight => (format!("main_w-w-{MARGIN_PX}"), format!("{MARGIN_PX}")),
        WebcamCorner::BottomLeft => (format!("{MARGIN_PX}"), format!("main_h-h-{MARGIN_PX}")),
        WebcamCorner::BottomRight => {
            (format!("main_w-w-{MARGIN_PX}"), format!("main_h-h-{MARGIN_PX}"))
        }
    };

    format!(
        "[{cam_index}:v]crop='min(iw,ih)':'min(iw,ih)'[camsq];\
         [camsq][0:v]scale2ref=w=main_w*{SIZE_FRACTION}:h=main_w*{SIZE_FRACTION}[camscaled][mainref];\
         {mask};\
         [mainref][camfinal]overlay={x}:{y}[outv]"
    )
}

#[cfg(not(target_os = "windows"))]
pub fn build_segment_command(_config: &CaptureConfig, _output_path: &Path) -> Command {
    unimplemented!(
        "screen capture is only implemented for Windows in this prototype (Week 1 milestone scope, see roadmap in screen-recorder-spec.md)"
    )
}

/// Muxes a recorded segment with its separately-captured system-audio track
/// (raw interleaved f32le PCM, see `system_audio.rs`). If the segment has no
/// microphone track, the loopback audio becomes the sole audio track;
/// otherwise the two are mixed together.
pub fn build_mux_command(
    segment_video_path: &Path,
    loopback_pcm_path: &Path,
    has_mic_track: bool,
    output_path: &Path,
) -> Command {
    let mut cmd = Command::new(crate::setup::paths::ffmpeg_program());
    cmd.arg("-y")
        .arg("-i")
        .arg(segment_video_path)
        .arg("-f")
        .arg("f32le")
        .arg("-ar")
        .arg(system_audio::SAMPLE_RATE.to_string())
        .arg("-ac")
        .arg(system_audio::CHANNELS.to_string())
        .arg("-i")
        .arg(loopback_pcm_path);

    if has_mic_track {
        cmd.arg("-filter_complex")
            .arg("[0:a][1:a]amix=inputs=2:duration=longest[aout]")
            .arg("-map")
            .arg("0:v")
            .arg("-map")
            .arg("[aout]");
    } else {
        cmd.arg("-map").arg("0:v").arg("-map").arg("1:a");
    }

    cmd.arg("-c:v")
        .arg("copy")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("160k")
        .arg(output_path);

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Builds the live-subtitles resampler (F6): a long-lived ffmpeg process
/// that downmixes/downsamples the raw loopback tap (48kHz stereo f32le) to
/// what `whisper_worker.py --live` expects (16kHz mono f32le), reading raw
/// PCM from stdin and writing raw PCM to stdout. Kept as ffmpeg rather than
/// hand-rolled Rust DSP, matching this app's "FFmpeg as a subprocess" rule.
/// Closing stdin makes ffmpeg flush and exit, which is how the caller shuts
/// the whole live-subtitles pipeline down.
pub fn build_resample_command() -> Command {
    let mut cmd = Command::new(crate::setup::paths::ffmpeg_program());
    cmd.arg("-y")
        .arg("-f")
        .arg("f32le")
        .arg("-ar")
        .arg(system_audio::SAMPLE_RATE.to_string())
        .arg("-ac")
        .arg(system_audio::CHANNELS.to_string())
        .arg("-i")
        .arg("pipe:0")
        .arg("-f")
        .arg("f32le")
        .arg("-ar")
        .arg("16000")
        .arg("-ac")
        .arg("1")
        .arg("pipe:1");

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Concatenates finalized segments (already muxed with all their audio) into
/// the final recording, via ffmpeg's concat demuxer with stream copy.
pub fn build_concat_command(list_file: &Path, output_path: &Path) -> Command {
    let mut cmd = Command::new(crate::setup::paths::ffmpeg_program());
    cmd.arg("-y")
        .arg("-f")
        .arg("concat")
        .arg("-safe")
        .arg("0")
        .arg("-i")
        .arg(list_file)
        .arg("-c")
        .arg("copy")
        .arg(output_path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Writes the file list consumed by `build_concat_command`. ffmpeg's concat
/// demuxer expects `file '<path>'` lines with single quotes escaped.
pub fn write_concat_list(segment_paths: &[std::path::PathBuf], list_path: &Path) -> Result<()> {
    let mut contents = String::new();
    for path in segment_paths {
        let escaped = path.to_string_lossy().replace('\'', "'\\''");
        contents.push_str(&format!("file '{escaped}'\n"));
    }
    std::fs::write(list_path, contents)
        .with_context(|| format!("failed to write concat list at {list_path:?}"))?;
    Ok(())
}

/// Extracts a normalized 16kHz mono WAV from a finished recording, for
/// handing to the STT worker (see `pipeline/stt.rs`). Every Whisper runtime
/// CLAUDE.md lists (faster-whisper here, whisper.cpp on other platforms)
/// takes a plain audio file, so the worker boundary stays swappable instead
/// of depending on a particular runtime's own container-decoding path.
pub fn build_wav_extract_command(video_path: &Path, wav_path: &Path) -> Command {
    let mut cmd = Command::new(crate::setup::paths::ffmpeg_program());
    cmd.arg("-y")
        .arg("-i")
        .arg(video_path)
        .arg("-vn")
        .arg("-ar")
        .arg("16000")
        .arg("-ac")
        .arg("1")
        .arg("-c:a")
        .arg("pcm_s16le")
        .arg(wav_path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Reads the finished recording's actual media duration via `ffprobe`
/// (rather than tracking wall-clock time across pause/resume segments in the
/// controller), so it reflects the real duration of the concatenated output
/// file. Used to populate the recordings library (F5).
pub async fn probe_duration_ms(video_path: &Path) -> Result<i64> {
    let output = Command::new(crate::setup::paths::ffprobe_program())
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(video_path)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to spawn ffprobe")?;
    if !output.status.success() {
        return Err(anyhow!(
            "ffprobe exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let seconds: f64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .with_context(|| format!("failed to parse ffprobe duration output for {video_path:?}"))?;
    Ok((seconds * 1000.0).round() as i64)
}

/// Enumerates dshow audio input device names (microphones) by parsing
/// ffmpeg's `-list_devices` stderr output. Used to populate the microphone
/// picker instead of hardcoding a device name.
#[cfg(target_os = "windows")]
pub async fn list_microphones() -> Result<Vec<String>> {
    let output = Command::new(crate::setup::paths::ffmpeg_program())
        .args(["-hide_banner", "-f", "dshow", "-list_devices", "true", "-i", "dummy"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run ffmpeg -list_devices")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut devices = Vec::new();
    for line in stderr.lines() {
        // ffmpeg prefixes each line with a `[in#0 @ <addr>]`-style context
        // tag, so the quoted device name never starts the line — search for
        // the quotes rather than requiring them at position 0.
        if !line.contains("(audio)") {
            continue;
        }
        if let Some(start) = line.find('"') {
            if let Some(len) = line[start + 1..].find('"') {
                devices.push(line[start + 1..start + 1 + len].to_string());
            }
        }
    }
    Ok(devices)
}

#[cfg(not(target_os = "windows"))]
pub async fn list_microphones() -> Result<Vec<String>> {
    Ok(Vec::new())
}

/// Enumerates dshow video input device names (webcams), for F16's camera
/// picker. Mirrors `list_microphones` exactly, filtering on `"(video)"`
/// instead of `"(audio)"` in the same `-list_devices` stderr output.
#[cfg(target_os = "windows")]
pub async fn list_cameras() -> Result<Vec<String>> {
    let output = Command::new(crate::setup::paths::ffmpeg_program())
        .args(["-hide_banner", "-f", "dshow", "-list_devices", "true", "-i", "dummy"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run ffmpeg -list_devices")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut devices = Vec::new();
    for line in stderr.lines() {
        if !line.contains("(video)") {
            continue;
        }
        if let Some(start) = line.find('"') {
            if let Some(len) = line[start + 1..].find('"') {
                devices.push(line[start + 1..start + 1 + len].to_string());
            }
        }
    }
    Ok(devices)
}

#[cfg(not(target_os = "windows"))]
pub async fn list_cameras() -> Result<Vec<String>> {
    Ok(Vec::new())
}
