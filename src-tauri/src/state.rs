use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use tauri_plugin_global_shortcut::Shortcut;

use crate::capture::CaptureController;
use crate::db::SharedConn;
use crate::pipeline::live_subtitles::LiveSubtitles;
use crate::pipeline::{JobSender, TranslationControl};

/// Recording state is a single optional controller: this prototype supports
/// one active recording at a time, matching the F1 acceptance criteria
/// (start/pause/resume/stop on a single in-progress recording).
pub struct AppState {
    pub controller: Mutex<Option<CaptureController>>,
    /// F6 live subtitles: the running resampler/daemon pipeline for the
    /// current recording, if the feature is enabled and system audio
    /// capture is on. Same one-at-a-time shape as `controller`.
    pub live_subtitles: Mutex<Option<LiveSubtitles>>,
    /// Base folder new recordings are saved under; each recording gets its
    /// own subfolder within it (see `commands::stop_recording`). User-
    /// changeable at runtime via `commands::set_recordings_dir`, so it's
    /// behind a lock rather than a plain `PathBuf`.
    pub recordings_dir: Mutex<PathBuf>,
    pub db: SharedConn,
    pub job_tx: JobSender,
    /// One entry per in-flight `translate` job, keyed by recording id --
    /// see `pipeline::TranslationControl` and `commands::pause_translation`/
    /// `resume_translation`/`cancel_translation`.
    pub translation_controls: Mutex<HashMap<String, Arc<TranslationControl>>>,
    /// The currently-registered OS-level hotkey for `overlay::toggle`, so
    /// `hotkeys::apply` can unregister it before registering a replacement
    /// when the user changes the setting (see `commands::set_hotkey_settings`).
    pub active_hotkey: Mutex<Option<Shortcut>>,
    /// Recording ids with a `compress_recording` job currently running --
    /// guards against a second click starting a competing HandBrakeCLI
    /// process against the same output file (see `commands::compress_recording`).
    pub compressing: Mutex<HashSet<String>>,
}
