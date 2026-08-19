use serde::{Deserialize, Serialize};

/// User-facing capture configuration for a single recording.
///
/// Week 1 scope: full-screen video only (window/region picking is a later
/// milestone per screen-recorder-spec.md F1). Each audio source can be
/// disabled independently, matching the F1 acceptance criteria.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureConfig {
    pub capture_system_audio: bool,
    pub capture_microphone: bool,
    /// dshow device name, e.g. "Набор микрофонов (Realtek(R) Audio)".
    /// `None` lets the OS-default microphone be used.
    pub microphone_device: Option<String>,
    pub framerate: u32,
    /// Whether this recording captures desktop video at all. `false` records
    /// audio only (system audio and/or microphone, whichever is enabled),
    /// producing an audio-only .mp4 with no video stream -- see
    /// `ffmpeg::build_segment_command`. Defaults to `true` so existing
    /// screen-recording behavior is unaffected. The webcam overlay requires
    /// video and is ignored when this is `false` (see
    /// `CaptureController::begin_segment`).
    #[serde(default = "default_capture_video")]
    pub capture_video: bool,
    /// F16 webcam picture-in-picture overlay, composited live into the
    /// desktop capture (see `ffmpeg::build_segment_command`). Disabled by
    /// default so a plain screen recording is unaffected.
    #[serde(default)]
    pub webcam: WebcamOverlayConfig,
}

fn default_capture_video() -> bool {
    true
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            capture_system_audio: true,
            capture_microphone: true,
            microphone_device: None,
            framerate: 30,
            capture_video: true,
            webcam: WebcamOverlayConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebcamShape {
    Circle,
    Square,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebcamCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// F16: an optional small webcam feed composited into a corner of the
/// desktop capture. `device: None` auto-resolves to the first camera
/// `ffmpeg::list_cameras` finds, mirroring how `microphone_device` is
/// resolved in `CaptureController::begin_segment`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebcamOverlayConfig {
    pub enabled: bool,
    pub device: Option<String>,
    pub shape: WebcamShape,
    pub corner: WebcamCorner,
}

impl Default for WebcamOverlayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            device: None,
            shape: WebcamShape::Circle,
            corner: WebcamCorner::BottomRight,
        }
    }
}
