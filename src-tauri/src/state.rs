use std::path::PathBuf;

use tokio::sync::Mutex;

use crate::capture::CaptureController;
use crate::db::SharedConn;
use crate::pipeline::JobSender;

/// Recording state is a single optional controller: this prototype supports
/// one active recording at a time, matching the F1 acceptance criteria
/// (start/pause/resume/stop on a single in-progress recording).
pub struct AppState {
    pub controller: Mutex<Option<CaptureController>>,
    /// Base folder new recordings are saved under; each recording gets its
    /// own subfolder within it (see `commands::stop_recording`). User-
    /// changeable at runtime via `commands::set_recordings_dir`, so it's
    /// behind a lock rather than a plain `PathBuf`.
    pub recordings_dir: Mutex<PathBuf>,
    pub db: SharedConn,
    pub job_tx: JobSender,
}
