pub mod config;
pub mod controller;
pub mod ffmpeg;
pub mod system_audio;

pub use config::CaptureConfig;
pub use controller::{CaptureController, RecordingStatus};
