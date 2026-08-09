use anyhow::Result;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "settings";

/// Shows the Settings window (Speech recognition / Hotkeys / LLM provider
/// tabs — see `SettingsWindow.tsx`). Creates it on first use, then just
/// re-shows and focuses it on subsequent calls, mirroring `overlay::show`.
pub fn show(app: &AppHandle) -> Result<()> {
    match app.get_webview_window(LABEL) {
        Some(w) => {
            w.show()?;
            w.set_focus()?;
        }
        None => {
            WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
                .title("Settings")
                .inner_size(640.0, 560.0)
                .resizable(true)
                .build()?;
        }
    }
    Ok(())
}
