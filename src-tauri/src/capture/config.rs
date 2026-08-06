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
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            capture_system_audio: true,
            capture_microphone: true,
            microphone_device: None,
            framerate: 30,
        }
    }
}
