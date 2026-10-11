use anyhow::Result;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "live-subtitles";

/// Shows the live-subtitles window: a scrollable history of every caption
/// emitted over `live-subtitle-caption` so far this recording session (see
/// `LiveSubtitlesWindow.tsx` and `state::AppState::live_caption_history`),
/// as opposed to the main window's single-line "latest caption" preview.
/// Creates it on first use, then just re-shows and focuses it on subsequent
/// calls, mirroring `settings_window::show`.
pub fn show(app: &AppHandle) -> Result<()> {
    match app.get_webview_window(LABEL) {
        Some(w) => {
            w.show()?;
            w.set_focus()?;
        }
        None => {
            WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
                .title("Live subtitles")
                .inner_size(480.0, 640.0)
                .resizable(true)
                .build()?;
        }
    }
    Ok(())
}
