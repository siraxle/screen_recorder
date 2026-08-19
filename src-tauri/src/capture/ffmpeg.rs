use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

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

/// Last number of stderr lines kept by `spawn_stderr_capture`, enough to
/// cover ffmpeg's actual error output (which usually lands in the final few
/// lines before it exits) without holding a whole run's progress spam.
const STDERR_TAIL_LINES: usize = 20;

/// Like `spawn_stderr_drain`, but also keeps the last `STDERR_TAIL_LINES`
/// lines in memory so a failing process's real error can be attached to the
/// `Result` we return on non-zero exit. ffmpeg's failure reason only ever
/// appears on stderr, and `spawn_stderr_drain` logs it at `debug!` -- below
/// the default `info` filter (see `logging::env_filter`) -- so without this,
/// a crash surfaces only as a bare, unexplained exit code.
pub fn spawn_stderr_capture(
    stderr: ChildStderr,
    label: String,
) -> (tokio::task::JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
    let tail = Arc::new(Mutex::new(Vec::new()));
    let tail_writer = tail.clone();
    let handle = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    tracing::debug!(target: "ffmpeg", "[{label}] {line}");
                    let mut buf = tail_writer.lock().unwrap();
                    buf.push(line);
                    let excess = buf.len().saturating_sub(STDERR_TAIL_LINES);
                    if excess > 0 {
                        buf.drain(0..excess);
                    }
                }
                _ => break,
            }
        }
    });
    (handle, tail)
}

/// Joins the captured tail lines into a single string for error messages.
pub fn stderr_tail(tail: &Mutex<Vec<String>>) -> String {
    tail.lock().unwrap().join("\n")
}

/// Whether ffmpeg's stderr shows it finished writing the output file (the
/// final `Lsize=` progress line, printed once when the muxer closes)
/// *despite* the process then exiting with a non-zero status.
///
/// Quitting a process that combines gdigrab + a dshow audio device (see
/// `build_segment_command`) via `q` is a known Windows race: the dshow
/// input thread can report a transient `EAGAIN` after the muxer has already
/// flushed and closed the file, which makes ffmpeg's own exit code reflect
/// that error even though the output is complete and playable. Treating
/// every non-zero exit as fatal in that case would discard footage that
/// already exists on disk.
pub fn finished_writing_output(stderr_tail: &str) -> bool {
    stderr_tail.contains("Lsize=")
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

/// Builds the ffmpeg process that records one segment's desktop video via
/// gdigrab, plus the microphone via dshow if enabled. System audio is
/// captured separately (see `system_audio.rs`, WASAPI has no ffmpeg/dshow
/// path on stock Windows) and muxed in afterwards by `build_mux_command`.
///
/// This is deliberately webcam-agnostic (see `build_webcam_segment_command`
/// and `build_webcam_composite_command` for F16): compositing the webcam
/// live, in the same process, meant this filter graph had to keep pace with
/// two independently-paced real-time devices (gdigrab + dshow) or the whole
/// recording would stutter waiting on whichever device was slower. Capturing
/// each device into its own plain, filter-free process and compositing them
/// together *after* the segment stops removes that real-time coupling
/// entirely, so a plain screen recording (and, transitively, the desktop
/// track of a webcam recording) is unaffected by the feature's existence.
///
/// `+frag_keyframe+empty_moov` makes the mp4 playable even if the process is
/// killed instead of stopped gracefully — segment boundaries (pause/resume)
/// must never corrupt already-recorded footage.
/// Only called when `config.capture_video || config.capture_microphone`
/// (see `CaptureController::begin_segment`) -- an audio-only recording with
/// just system audio never spawns this process at all, since the WASAPI
/// loopback capture (`system_audio.rs`) runs independently of ffmpeg.
#[cfg(target_os = "windows")]
pub fn build_segment_command(config: &CaptureConfig, output_path: &Path) -> Command {
    let mut cmd = crate::procutil::command(crate::setup::paths::ffmpeg_program());
    cmd.arg("-y");

    if config.capture_video {
        cmd.arg("-f").arg("gdigrab");
        if config.webcam.enabled {
            // Only needed when a webcam track will later be composited
            // against this one (see `build_webcam_composite_command`) --
            // tags each frame with real wall-clock time instead of an
            // assumed-continuous frame-index PTS, which is what lets the
            // offline compose pass align two independently-started capture
            // processes correctly.
            cmd.arg("-use_wallclock_as_timestamps").arg("1");
        }
        cmd.arg("-framerate")
            .arg(config.framerate.to_string())
            .arg("-i")
            .arg("desktop");
    }

    if config.capture_microphone {
        let mic = config
            .microphone_device
            .as_deref()
            .expect("capture_microphone requires a resolved microphone_device");
        cmd.arg("-thread_queue_size")
            .arg("1024")
            .arg("-f")
            .arg("dshow")
            .arg("-i")
            .arg(format!("audio={mic}"));
    }

    if config.capture_video {
        cmd.arg("-c:v")
            .arg("libx264")
            .arg("-preset")
            .arg("ultrafast")
            .arg("-pix_fmt")
            .arg("yuv420p");
    }
    cmd.arg("-movflags").arg("+frag_keyframe+empty_moov");

    if config.capture_microphone {
        cmd.arg("-c:a").arg("aac").arg("-b:a").arg("160k");
    }

    cmd.arg(output_path);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Builds the F16 webcam capture process: just the raw camera feed via
/// dshow, encoded with no filter graph, run as its own process alongside
/// `build_segment_command`'s desktop capture. Kept maximally simple (single
/// input, no filters) so it can never be the thing that makes the *desktop*
/// capture fall behind -- the two devices no longer share a process or a
/// filter graph at all, only a compositing pass after they've both stopped
/// (see `build_webcam_composite_command`).
#[cfg(target_os = "windows")]
pub fn build_webcam_segment_command(config: &CaptureConfig, output_path: &Path) -> Command {
    let device = config
        .webcam
        .device
        .as_deref()
        .expect("webcam.enabled requires a resolved webcam.device");
    let mut cmd = crate::procutil::command(crate::setup::paths::ffmpeg_program());
    cmd.arg("-y")
        .arg("-thread_queue_size")
        .arg("1024")
        .arg("-use_wallclock_as_timestamps")
        .arg("1")
        .arg("-f")
        .arg("dshow")
        .arg("-framerate")
        .arg(config.framerate.to_string())
        .arg("-i")
        .arg(format!("video={device}"))
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("ultrafast")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-movflags")
        .arg("+frag_keyframe+empty_moov")
        .arg(output_path);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Builds the F16 offline compositing pass: takes the already-finished
/// desktop segment and webcam segment (see `build_segment_command` and
/// `build_webcam_segment_command`) and merges them into one video -- crops
/// the webcam feed to a square, optionally masks it into a circle, scales it
/// to a fixed fraction of the desktop's own width (via `scale2ref`, so this
/// never needs to query the desktop's actual resolution), and overlays it
/// onto the desktop video in the configured corner with a fixed margin.
///
/// Runs *after* the segment stops, not live, so unlike the old single-process
/// design it isn't racing either device's real-time frame delivery -- ffmpeg
/// processes both files as fast as the CPU allows, which is what actually
/// fixes the choppiness (the live version could stall the whole recording
/// waiting on whichever of gdigrab/dshow produced its next frame slower).
///
/// `desktop_offset_secs`/`webcam_offset_secs` come from wall-clock instants
/// the caller recorded immediately before spawning each process (see
/// `CaptureController::begin_segment`); combined with
/// `-use_wallclock_as_timestamps` on both live captures and `-copyts` here,
/// they align the two independently-started files to a shared time base
/// instead of assuming both happened to start at the same instant (dshow
/// device negotiation is often a few hundred ms slower than gdigrab).
///
/// Masking happens before the final upscale, not after, so the per-pixel
/// `geq` circle mask always runs on a small `MASK_SIZE`-square frame instead
/// of the full displayed overlay size -- cheap and constant-cost regardless
/// of desktop resolution or the configured size fraction, which matters more
/// now that this can run at a higher-quality preset without a real-time
/// budget to protect.
#[cfg(target_os = "windows")]
pub fn build_webcam_composite_command(
    desktop_path: &Path,
    webcam_path: &Path,
    webcam: &WebcamOverlayConfig,
    desktop_offset_secs: f64,
    webcam_offset_secs: f64,
    has_mic_track: bool,
    output_path: &Path,
) -> Command {
    let mut cmd = crate::procutil::command(crate::setup::paths::ffmpeg_program());
    cmd.arg("-y").arg("-copyts");
    if desktop_offset_secs > 0.0 {
        cmd.arg("-itsoffset").arg(format!("{desktop_offset_secs:.3}"));
    }
    cmd.arg("-i").arg(desktop_path);
    if webcam_offset_secs > 0.0 {
        cmd.arg("-itsoffset").arg(format!("{webcam_offset_secs:.3}"));
    }
    cmd.arg("-i").arg(webcam_path);

    cmd.arg("-filter_complex").arg(build_webcam_overlay_filter(webcam));
    cmd.arg("-map").arg("[outv]");
    if has_mic_track {
        cmd.arg("-map").arg("0:a");
    }

    cmd.arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("ultrafast")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-movflags")
        .arg("+frag_keyframe+empty_moov");
    if has_mic_track {
        cmd.arg("-c:a").arg("copy");
    }
    cmd.arg(output_path);

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Builds the F16 webcam-overlay filter graph used by
/// `build_webcam_composite_command`. Input 0 is always the desktop segment
/// and input 1 is always the webcam segment -- fixed indices, since (unlike
/// the old single live process) the two are always separate files composed
/// in a known order.
#[cfg(target_os = "windows")]
fn build_webcam_overlay_filter(webcam: &WebcamOverlayConfig) -> String {
    // Fixed rather than user-configurable -- the user asked for corner and
    // shape to be pickable, not a size slider; both can become settings
    // later if wanted.
    const SIZE_FRACTION: &str = "0.16";
    const MARGIN_PX: i32 = 8;
    // Resolution the circle mask is computed at, independent of the final
    // displayed size -- keeps the geq cost constant regardless of desktop
    // resolution or SIZE_FRACTION.
    const MASK_SIZE: i32 = 256;

    let mask = match webcam.shape {
        // Hard-edge circular alpha mask (no feathering) -- computed at the
        // small, fixed MASK_SIZE so cost doesn't scale with desktop
        // resolution or the configured overlay size.
        WebcamShape::Circle => {
            format!(
                "[camsq]scale={MASK_SIZE}:{MASK_SIZE}[cammasksrc];\
                 [cammasksrc]format=yuva420p,geq=lum='p(X,Y)':a='if(lte(pow(X-W/2,2)+pow(Y-H/2,2),pow(W/2,2)),255,0)'[camfinal]"
            )
        }
        WebcamShape::Square => "[camsq]copy[camfinal]".to_string(),
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
        "[1:v]crop='min(iw,ih)':'min(iw,ih)'[camsq];\
         {mask};\
         [camfinal][0:v]scale2ref=w=ref_w*{SIZE_FRACTION}:h=ref_w*{SIZE_FRACTION}[camscaled][mainref];\
         [mainref][camscaled]overlay={x}:{y}[outv]"
    )
}

#[cfg(not(target_os = "windows"))]
pub fn build_segment_command(_config: &CaptureConfig, _output_path: &Path) -> Command {
    unimplemented!(
        "screen capture is only implemented for Windows in this prototype (Week 1 milestone scope, see roadmap in screen-recorder-spec.md)"
    )
}

#[cfg(not(target_os = "windows"))]
pub fn build_webcam_segment_command(_config: &CaptureConfig, _output_path: &Path) -> Command {
    unimplemented!(
        "screen capture is only implemented for Windows in this prototype (Week 1 milestone scope, see roadmap in screen-recorder-spec.md)"
    )
}

#[cfg(not(target_os = "windows"))]
pub fn build_webcam_composite_command(
    _desktop_path: &Path,
    _webcam_path: &Path,
    _webcam: &WebcamOverlayConfig,
    _desktop_offset_secs: f64,
    _webcam_offset_secs: f64,
    _has_mic_track: bool,
    _output_path: &Path,
) -> Command {
    unimplemented!(
        "screen capture is only implemented for Windows in this prototype (Week 1 milestone scope, see roadmap in screen-recorder-spec.md)"
    )
}

/// Muxes a recorded segment with its separately-captured system-audio track
/// (raw interleaved f32le PCM, see `system_audio.rs`). If the segment has no
/// microphone track, the loopback audio becomes the sole audio track;
/// otherwise the two are mixed together. `has_video` is `false` for an
/// audio-only recording (see `CaptureConfig::capture_video`) whose primary
/// input is a mic-only capture with no video stream to map or copy.
pub fn build_mux_command(
    primary_path: &Path,
    loopback_pcm_path: &Path,
    has_video: bool,
    has_mic_track: bool,
    output_path: &Path,
) -> Command {
    let mut cmd = crate::procutil::command(crate::setup::paths::ffmpeg_program());
    cmd.arg("-y")
        .arg("-i")
        .arg(primary_path)
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
            .arg("[0:a][1:a]amix=inputs=2:duration=longest[aout]");
        if has_video {
            cmd.arg("-map").arg("0:v");
        }
        cmd.arg("-map").arg("[aout]");
    } else if has_video {
        cmd.arg("-map").arg("0:v").arg("-map").arg("1:a");
    } else {
        cmd.arg("-map").arg("1:a");
    }

    if has_video {
        cmd.arg("-c:v").arg("copy");
    }
    cmd.arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("160k")
        .arg(output_path);

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Encodes the loopback-only PCM audio (see `system_audio.rs`) straight into
/// the segment's output container, for an audio-only recording (see
/// `CaptureConfig::capture_video`) that has system audio as its *sole*
/// source -- no primary ffmpeg process exists at all in that case (see
/// `CaptureController::begin_segment`), so there's nothing to mux against.
pub fn build_audio_only_mux_command(loopback_pcm_path: &Path, output_path: &Path) -> Command {
    let mut cmd = crate::procutil::command(crate::setup::paths::ffmpeg_program());
    cmd.arg("-y")
        .arg("-f")
        .arg("f32le")
        .arg("-ar")
        .arg(system_audio::SAMPLE_RATE.to_string())
        .arg("-ac")
        .arg(system_audio::CHANNELS.to_string())
        .arg("-i")
        .arg(loopback_pcm_path)
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
    let mut cmd = crate::procutil::command(crate::setup::paths::ffmpeg_program());
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
    let mut cmd = crate::procutil::command(crate::setup::paths::ffmpeg_program());
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
    let mut cmd = crate::procutil::command(crate::setup::paths::ffmpeg_program());
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
    let output = crate::procutil::command(crate::setup::paths::ffprobe_program())
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
    let output = crate::procutil::command(crate::setup::paths::ffmpeg_program())
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
    let output = crate::procutil::command(crate::setup::paths::ffmpeg_program())
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
