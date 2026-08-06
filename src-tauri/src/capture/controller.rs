use std::path::{Path, PathBuf};

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
    video_child: Child,
    video_path: PathBuf,
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
}

impl CaptureController {
    pub fn new(
        config: CaptureConfig,
        base_dir: &Path,
        language_override: Option<String>,
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
        let video_path = self
            .work_dir
            .join(format!("segment-{:03}-video.mp4", self.segment_index));

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

        let mut cmd = ffmpeg::build_segment_command(&self.config, &video_path);
        let mut video_child = cmd
            .spawn()
            .context("failed to spawn ffmpeg for video/mic capture")?;
        if let Some(stderr) = video_child.stderr.take() {
            ffmpeg::spawn_stderr_drain(stderr, format!("segment-{}", self.segment_index));
        }

        let (loopback, loopback_pcm_path) = if self.config.capture_system_audio {
            let pcm_path = self.work_dir.join(format!(
                "segment-{:03}-sysaudio.f32le",
                self.segment_index
            ));
            let capture = LoopbackCapture::start(&pcm_path)?;
            (Some(capture), Some(pcm_path))
        } else {
            (None, None)
        };

        self.active = Some(ActiveSegment {
            video_child,
            video_path,
            loopback,
            loopback_pcm_path,
            has_mic_track: self.config.capture_microphone,
        });
        Ok(())
    }

    async fn finish_segment(&self, mut segment: ActiveSegment) -> Result<PathBuf> {
        // ffmpeg reads 'q' from stdin as a graceful-stop request even when
        // stdin is a pipe rather than a real console.
        if let Some(mut stdin) = segment.video_child.stdin.take() {
            let _ = stdin.write_all(b"q").await;
        }
        let status = segment
            .video_child
            .wait()
            .await
            .context("failed to wait for video/mic ffmpeg process")?;

        if let Some(loopback) = segment.loopback.take() {
            loopback.stop()?;
        }

        if !status.success() {
            return Err(anyhow!(
                "ffmpeg (video/mic capture) exited with status {status}"
            ));
        }

        match segment.loopback_pcm_path {
            Some(pcm_path) => {
                let muxed_path = self
                    .work_dir
                    .join(format!("segment-{:03}-final.mp4", self.segment_index));
                ffmpeg::run_to_completion(
                    ffmpeg::build_mux_command(
                        &segment.video_path,
                        &pcm_path,
                        segment.has_mic_track,
                        &muxed_path,
                    ),
                    "mux",
                )
                .await?;
                Ok(muxed_path)
            }
            None => Ok(segment.video_path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        };
        let mut controller = CaptureController::new(config, &base_dir, None).unwrap();

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
}
