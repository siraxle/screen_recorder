use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use uuid::Uuid;

use super::config::CaptureConfig;
use super::ffmpeg;
use super::system_audio::LoopbackCapture;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStatus {
    Recording,
    Paused,
    Stopped,
}

/// One pause/resume cycle records into its own segment file, since ffmpeg
/// has no built-in pause. Segments are concatenated back together on stop —
/// this is what "pause without losing the already-recorded fragment" means
/// in practice (see F1 in screen-recorder-spec.md).
struct ActiveSegment {
    /// The gdigrab/dshow ffmpeg process for this segment's desktop video
    /// and/or microphone, or `None` when neither is being captured -- an
    /// audio-only recording (see `CaptureConfig::capture_video`) with only
    /// system audio enabled needs no ffmpeg process here at all, since the
    /// WASAPI loopback capture (`loopback` below) runs independently.
    primary_child: Option<Child>,
    primary_path: Option<PathBuf>,
    video_start: Instant,
    /// Tail of the primary ffmpeg process's stderr, kept so a non-zero exit
    /// can be reported with its actual error instead of a bare status code
    /// (see `ffmpeg::spawn_stderr_capture`).
    primary_stderr: Arc<Mutex<Vec<String>>>,
    /// F16: the webcam feed is captured by its own plain, filter-free ffmpeg
    /// process (see `ffmpeg::build_webcam_segment_command`) rather than
    /// composited live alongside the desktop capture -- two independent
    /// real-time devices sharing one filter graph is what made the old
    /// design stutter waiting on whichever was slower. `webcam_start` is
    /// used to align the two files in the post-stop compositing pass (see
    /// `finish_segment`).
    webcam: Option<(Child, PathBuf, Instant, Arc<Mutex<Vec<String>>>)>,
    loopback: Option<LoopbackCapture>,
    loopback_pcm_path: Option<PathBuf>,
    has_mic_track: bool,
}

pub struct CaptureController {
    config: CaptureConfig,
    work_dir: PathBuf,
    segment_index: u32,
    finalized_segments: Vec<PathBuf>,
    active: Option<ActiveSegment>,
    status: RecordingStatus,
    /// Transcription-language override for this recording, carried through
    /// from `start` to `stop` so it can be persisted alongside the finished
    /// recording (see `commands::stop_recording`). `None` means auto-detect.
    language_override: Option<String>,
    /// F6 live subtitles: when present, handed to `LoopbackCapture::start`
    /// on every segment so the live-captioning pipeline keeps receiving
    /// audio across pause/resume cycles. Set once at construction by
    /// `commands::start_recording`; dropping it (when the whole controller
    /// drops) is part of how that pipeline shuts down, see
    /// `pipeline::live_subtitles::LiveSubtitles::stop`.
    live_tap: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
}

impl CaptureController {
    pub fn new(
        config: CaptureConfig,
        base_dir: &Path,
        language_override: Option<String>,
        live_tap: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    ) -> Result<Self> {
        let work_dir = base_dir.join(format!("recording-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&work_dir)
            .with_context(|| format!("failed to create working dir {work_dir:?}"))?;
        Ok(Self {
            config,
            work_dir,
            segment_index: 0,
            finalized_segments: Vec::new(),
            active: None,
            status: RecordingStatus::Stopped,
            language_override,
            live_tap,
        })
    }

    pub fn status(&self) -> RecordingStatus {
        self.status
    }

    pub fn language_override(&self) -> Option<&str> {
        self.language_override.as_deref()
    }

    pub async fn start(&mut self) -> Result<()> {
        if self.active.is_some() {
            return Err(anyhow!("a segment is already recording"));
        }
        self.begin_segment().await?;
        self.status = RecordingStatus::Recording;
        Ok(())
    }

    pub async fn pause(&mut self) -> Result<()> {
        if self.status != RecordingStatus::Recording {
            return Err(anyhow!("not recording"));
        }
        let segment = self.active.take().ok_or_else(|| anyhow!("not recording"))?;
        let finalized = self.finish_segment(segment).await?;
        self.finalized_segments.push(finalized);
        self.status = RecordingStatus::Paused;
        Ok(())
    }

    pub async fn resume(&mut self) -> Result<()> {
        if self.status != RecordingStatus::Paused {
            return Err(anyhow!("not paused"));
        }
        self.begin_segment().await?;
        self.status = RecordingStatus::Recording;
        Ok(())
    }

    /// Finalizes the recording into `output_path` (mp4) and cleans up the
    /// working directory. The controller must not be reused afterwards.
    pub async fn stop(&mut self, output_path: &Path) -> Result<PathBuf> {
        if let Some(segment) = self.active.take() {
            let finalized = self.finish_segment(segment).await?;
            self.finalized_segments.push(finalized);
        }
        if self.finalized_segments.is_empty() {
            return Err(anyhow!("no footage was recorded"));
        }
        self.status = RecordingStatus::Stopped;

        if self.finalized_segments.len() == 1 {
            std::fs::copy(&self.finalized_segments[0], output_path)
                .context("failed to copy single segment to output path")?;
        } else {
            let list_path = self.work_dir.join("concat_list.txt");
            ffmpeg::write_concat_list(&self.finalized_segments, &list_path)?;
            ffmpeg::run_to_completion(
                ffmpeg::build_concat_command(&list_path, output_path),
                "concat",
            )
            .await?;
        }

        let _ = std::fs::remove_dir_all(&self.work_dir);
        Ok(output_path.to_path_buf())
    }

    async fn begin_segment(&mut self) -> Result<()> {
        self.segment_index += 1;

        if !self.config.capture_video
            && !self.config.capture_microphone
            && !self.config.capture_system_audio
        {
            return Err(anyhow!(
                "select at least one video or audio source before recording"
            ));
        }

        // dshow has no "OS default microphone" device name, so an unset
        // `microphone_device` is resolved here to a concrete device rather
        // than passed through as a placeholder string.
        if self.config.capture_microphone && self.config.microphone_device.is_none() {
            let mics = ffmpeg::list_microphones()
                .await
                .context("failed to enumerate microphones")?;
            self.config.microphone_device = Some(mics.into_iter().next().ok_or_else(|| {
                anyhow!("microphone capture is enabled but no microphone device was found")
            })?);
        }

        // Same auto-resolve as the microphone above, for F16's webcam
        // overlay: an unset `webcam.device` picks the first camera ffmpeg
        // enumerates. The webcam overlay is meaningless without desktop
        // video, so it's ignored outright in audio-only recordings rather
        // than trusting the caller never to send that combination.
        if self.config.capture_video && self.config.webcam.enabled && self.config.webcam.device.is_none() {
            let cameras = ffmpeg::list_cameras()
                .await
                .context("failed to enumerate cameras")?;
            self.config.webcam.device = Some(cameras.into_iter().next().ok_or_else(|| {
                anyhow!("webcam overlay is enabled but no camera device was found")
            })?);
        }

        // No ffmpeg process is needed here at all when neither desktop video
        // nor the microphone is being captured -- an audio-only recording
        // with just system audio relies solely on `loopback` below.
        let has_primary = self.config.capture_video || self.config.capture_microphone;
        let (primary_child, primary_path, primary_stderr) = if has_primary {
            let label = if self.config.capture_video { "video" } else { "audio" };
            let primary_path = self
                .work_dir
                .join(format!("segment-{:03}-{label}.mp4", self.segment_index));
            let mut cmd = ffmpeg::build_segment_command(&self.config, &primary_path);
            let mut child = cmd
                .spawn()
                .context("failed to spawn ffmpeg for video/mic capture")?;
            let stderr = child
                .stderr
                .take()
                .map(|stderr| ffmpeg::spawn_stderr_capture(stderr, format!("segment-{}", self.segment_index)).1)
                .unwrap_or_default();
            (Some(child), Some(primary_path), stderr)
        } else {
            (None, None, Arc::new(Mutex::new(Vec::new())))
        };
        let video_start = Instant::now();

        // F16: webcam is its own process (see `ActiveSegment::webcam` doc
        // comment) so it's spawned right alongside the desktop capture
        // rather than as a second input inside the same command.
        let webcam = if self.config.capture_video && self.config.webcam.enabled {
            let webcam_path = self
                .work_dir
                .join(format!("segment-{:03}-webcam.mp4", self.segment_index));
            let mut webcam_cmd = ffmpeg::build_webcam_segment_command(&self.config, &webcam_path);
            let webcam_start = Instant::now();
            let mut webcam_child = webcam_cmd
                .spawn()
                .context("failed to spawn ffmpeg for webcam capture")?;
            let webcam_stderr = webcam_child
                .stderr
                .take()
                .map(|stderr| {
                    ffmpeg::spawn_stderr_capture(stderr, format!("segment-{}-webcam", self.segment_index)).1
                })
                .unwrap_or_default();
            Some((webcam_child, webcam_path, webcam_start, webcam_stderr))
        } else {
            None
        };

        let (loopback, loopback_pcm_path) = if self.config.capture_system_audio {
            let pcm_path = self.work_dir.join(format!(
                "segment-{:03}-sysaudio.f32le",
                self.segment_index
            ));
            let capture = LoopbackCapture::start(&pcm_path, self.live_tap.clone())?;
            (Some(capture), Some(pcm_path))
        } else {
            (None, None)
        };

        self.active = Some(ActiveSegment {
            primary_child,
            primary_path,
            video_start,
            primary_stderr,
            webcam,
            loopback,
            loopback_pcm_path,
            has_mic_track: self.config.capture_microphone,
        });
        Ok(())
    }

    async fn finish_segment(&self, mut segment: ActiveSegment) -> Result<PathBuf> {
        // ffmpeg reads 'q' from stdin as a graceful-stop request even when
        // stdin is a pipe rather than a real console. There's nothing to stop
        // here when this segment captured only system audio (no video, no
        // mic) -- see `ActiveSegment::primary_child`'s doc comment.
        let primary_status = if let Some(mut child) = segment.primary_child.take() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(b"q").await;
            }
            Some(
                child
                    .wait()
                    .await
                    .context("failed to wait for video/mic ffmpeg process")?,
            )
        } else {
            None
        };

        if let Some(loopback) = segment.loopback.take() {
            loopback.stop()?;
        }

        if let Some(status) = primary_status {
            if !status.success() {
                let tail = ffmpeg::stderr_tail(&segment.primary_stderr);
                if ffmpeg::finished_writing_output(&tail) {
                    tracing::warn!(
                        "ffmpeg (video/mic capture) exited with status {status} after it finished \
                         writing the output file (known dshow-quit race on Windows) -- keeping the \
                         recording\n{tail}"
                    );
                } else {
                    return Err(anyhow!(
                        "ffmpeg (video/mic capture) exited with status {status}{}",
                        if tail.is_empty() { String::new() } else { format!("\n{tail}") }
                    ));
                }
            }
        }

        // F16: composite the webcam feed onto the desktop video now that
        // both have stopped, rather than live during capture -- see
        // `ffmpeg::build_webcam_composite_command` for why running this
        // offline (not racing either device's real-time frame delivery) is
        // what actually fixes the choppiness.
        let primary_path: Option<PathBuf> = match segment.webcam.take() {
            Some((mut webcam_child, webcam_path, webcam_start, webcam_stderr)) => {
                // The webcam overlay is only ever enabled alongside desktop
                // video capture (see `begin_segment`), so `primary_path` is
                // guaranteed to be set here.
                let desktop_path = segment
                    .primary_path
                    .clone()
                    .expect("webcam overlay requires desktop video capture");
                if let Some(mut stdin) = webcam_child.stdin.take() {
                    let _ = stdin.write_all(b"q").await;
                }
                let webcam_status = webcam_child
                    .wait()
                    .await
                    .context("failed to wait for webcam ffmpeg process")?;
                if !webcam_status.success() {
                    let tail = ffmpeg::stderr_tail(&webcam_stderr);
                    if ffmpeg::finished_writing_output(&tail) {
                        tracing::warn!(
                            "ffmpeg (webcam capture) exited with status {webcam_status} after it \
                             finished writing the output file (known dshow-quit race on Windows) \
                             -- keeping the recording\n{tail}"
                        );
                    } else {
                        return Err(anyhow!(
                            "ffmpeg (webcam capture) exited with status {webcam_status}{}",
                            if tail.is_empty() { String::new() } else { format!("\n{tail}") }
                        ));
                    }
                }

                // Neither process is guaranteed to start first -- gdigrab
                // usually starts almost immediately while dshow device
                // negotiation can lag by hundreds of ms, but that's not
                // guaranteed either. Whichever started later gets offset 0;
                // the earlier one is delayed by the measured gap so both
                // align to a shared time base in the compose pass.
                let min_start = segment.video_start.min(webcam_start);
                let desktop_offset = segment
                    .video_start
                    .saturating_duration_since(min_start)
                    .as_secs_f64();
                let webcam_offset = webcam_start.saturating_duration_since(min_start).as_secs_f64();

                let composited_path = self
                    .work_dir
                    .join(format!("segment-{:03}-composited.mp4", self.segment_index));
                ffmpeg::run_to_completion(
                    ffmpeg::build_webcam_composite_command(
                        &desktop_path,
                        &webcam_path,
                        &self.config.webcam,
                        desktop_offset,
                        webcam_offset,
                        segment.has_mic_track,
                        &composited_path,
                    ),
                    "webcam-composite",
                )
                .await?;
                Some(composited_path)
            }
            None => segment.primary_path.clone(),
        };

        match (primary_path, segment.loopback_pcm_path) {
            (Some(primary_path), Some(pcm_path)) => {
                let muxed_path = self
                    .work_dir
                    .join(format!("segment-{:03}-final.mp4", self.segment_index));
                ffmpeg::run_to_completion(
                    ffmpeg::build_mux_command(
                        &primary_path,
                        &pcm_path,
                        self.config.capture_video,
                        segment.has_mic_track,
                        &muxed_path,
                    ),
                    "mux",
                )
                .await?;
                Ok(muxed_path)
            }
            (Some(primary_path), None) => Ok(primary_path),
            (None, Some(pcm_path)) => {
                // Audio-only recording with system audio as its sole source
                // (no video, no mic) -- there's no `primary_path` at all to
                // mux against, just encode the loopback PCM straight into the
                // segment's output container.
                let output_path = self
                    .work_dir
                    .join(format!("segment-{:03}-final.mp4", self.segment_index));
                ffmpeg::run_to_completion(
                    ffmpeg::build_audio_only_mux_command(&pcm_path, &output_path),
                    "audio-mux",
                )
                .await?;
                Ok(output_path)
            }
            (None, None) => Err(anyhow!("segment produced no audio or video output")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::config::WebcamOverlayConfig;
    use std::time::Duration;

    /// Exercises the real pipeline end-to-end (spawns actual ffmpeg + a real
    /// WASAPI loopback capture) rather than mocking it, since the whole
    /// point of the Week 1 milestone is verifying capture actually works on
    /// this machine. Mic is left off so the test doesn't depend on a
    /// specific dshow device name being present in CI-like environments.
    #[tokio::test]
    async fn records_pauses_resumes_and_produces_playable_mp4() {
        let base_dir = std::env::temp_dir().join("screen_recorder_capture_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();

        let config = CaptureConfig {
            capture_system_audio: true,
            capture_microphone: false,
            microphone_device: None,
            framerate: 10,
            capture_video: true,
            webcam: WebcamOverlayConfig::default(),
        };
        let mut controller = CaptureController::new(config, &base_dir, None, None).unwrap();

        controller.start().await.expect("start should succeed");
        assert_eq!(controller.status(), RecordingStatus::Recording);
        tokio::time::sleep(Duration::from_secs(2)).await;

        controller.pause().await.expect("pause should succeed");
        assert_eq!(controller.status(), RecordingStatus::Paused);
        tokio::time::sleep(Duration::from_millis(300)).await;

        controller.resume().await.expect("resume should succeed");
        assert_eq!(controller.status(), RecordingStatus::Recording);
        tokio::time::sleep(Duration::from_secs(2)).await;

        let output_path = base_dir.join("output.mp4");
        let result = controller
            .stop(&output_path)
            .await
            .expect("stop should succeed and produce a file");

        assert_eq!(controller.status(), RecordingStatus::Stopped);
        assert!(result.exists(), "output file must exist");
        let size = std::fs::metadata(&result).unwrap().len();
        assert!(size > 1000, "output file is suspiciously small: {size} bytes");
    }

    /// F16: exercises the real webcam-overlay filter graph against an
    /// actual camera, rather than mocking ffmpeg -- but skips (not fails)
    /// when this machine has no camera attached, since dev/CI hardware
    /// varies (mirrors how the test above avoids depending on a specific
    /// mic device).
    #[tokio::test]
    async fn records_with_webcam_overlay_when_camera_present() {
        let cameras = ffmpeg::list_cameras().await.expect("list_cameras should not error");
        let Some(device) = cameras.into_iter().next() else {
            eprintln!("skipping records_with_webcam_overlay_when_camera_present: no camera found");
            return;
        };

        let base_dir = std::env::temp_dir().join("screen_recorder_webcam_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();

        let config = CaptureConfig {
            capture_system_audio: false,
            capture_microphone: false,
            microphone_device: None,
            framerate: 10,
            capture_video: true,
            webcam: WebcamOverlayConfig {
                enabled: true,
                device: Some(device),
                ..WebcamOverlayConfig::default()
            },
        };
        let mut controller = CaptureController::new(config, &base_dir, None, None).unwrap();

        controller.start().await.expect("start should succeed");
        tokio::time::sleep(Duration::from_secs(2)).await;

        let output_path = base_dir.join("output.mp4");
        let result = controller
            .stop(&output_path)
            .await
            .expect("stop should succeed and produce a file");

        assert!(result.exists(), "output file must exist");
        let size = std::fs::metadata(&result).unwrap().len();
        assert!(size > 1000, "output file is suspiciously small: {size} bytes");
    }

    /// Audio-only recording (`capture_video: false`) with system audio as
    /// the sole source -- exercises the `(None, Some(pcm_path))` branch of
    /// `finish_segment`, which has no ffmpeg process for desktop video/mic at
    /// all and instead encodes the WASAPI loopback PCM straight into the
    /// output container (see `ffmpeg::build_audio_only_mux_command`).
    #[tokio::test]
    async fn records_audio_only_with_system_audio_and_produces_playable_output() {
        let base_dir = std::env::temp_dir().join("screen_recorder_audio_only_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();

        let config = CaptureConfig {
            capture_system_audio: true,
            capture_microphone: false,
            microphone_device: None,
            framerate: 10,
            capture_video: false,
            webcam: WebcamOverlayConfig::default(),
        };
        let mut controller = CaptureController::new(config, &base_dir, None, None).unwrap();

        controller.start().await.expect("start should succeed");
        tokio::time::sleep(Duration::from_secs(2)).await;

        let output_path = base_dir.join("output.mp4");
        let result = controller
            .stop(&output_path)
            .await
            .expect("stop should succeed and produce a file");

        assert!(result.exists(), "output file must exist");
        let size = std::fs::metadata(&result).unwrap().len();
        assert!(size > 1000, "output file is suspiciously small: {size} bytes");
    }
}
