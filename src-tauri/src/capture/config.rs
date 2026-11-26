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
    /// F17 camera-only recording: record the camera full-frame instead of
    /// the desktop. A separate option from `capture_video` (which is about
    /// the *screen*) and from `webcam` (an overlay *on* the screen) -- when
    /// enabled it takes precedence over both, see
    /// `CaptureConfig::normalized`.
    #[serde(default)]
    pub camera_only: CameraOnlyConfig,
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
            camera_only: CameraOnlyConfig::default(),
        }
    }
}

impl CaptureConfig {
    /// Resolves combinations that can't coexist, so the controller never has
    /// to trust the caller to send a consistent config. F17: a camera-only
    /// recording has no desktop video, no webcam overlay (there's no screen
    /// to overlay onto), and records the microphone as its only audio
    /// source -- system audio is deliberately not captured in this mode.
    pub fn normalized(mut self) -> Self {
        if self.camera_only.enabled {
            self.capture_video = false;
            self.capture_system_audio = false;
            self.webcam.enabled = false;
        }
        self
    }
}

/// F17: camera-only recording. `device: None` auto-resolves to the first
/// camera `ffmpeg::list_cameras` finds, the same way `WebcamOverlayConfig`
/// does. The camera is recorded in its native (dshow default) mode and
/// mirrored horizontally, so the file looks the way the user sees
/// themselves -- see `ffmpeg::build_camera_only_segment_command`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraOnlyConfig {
    pub enabled: bool,
    pub device: Option<String>,
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
