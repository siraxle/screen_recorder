use anyhow::{anyhow, Result};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

const CANVAS_LABEL: &str = "annotation-canvas";
const TOOLBAR_LABEL: &str = "annotation-toolbar";
// Vertical toolbar docked to the left edge, tall enough to show every tool,
// color swatch and action button without clipping or scrolling.
const TOOLBAR_WIDTH: f64 = 88.0;
const TOOLBAR_HEIGHT: f64 = 460.0;
const TOOLBAR_MARGIN: f64 = 16.0;

/// Shows the annotation overlay: a transparent, click-through-by-default
/// drawing surface plus a small always-on-top toolbar, both scoped to the
/// primary monitor. Creates the windows on first use, then just re-shows
/// them on subsequent calls (see `hide`).
pub fn show(app: &AppHandle) -> Result<()> {
    let monitor = app
        .primary_monitor()?
        .ok_or_else(|| anyhow!("no primary monitor detected"))?;
    let scale = monitor.scale_factor();
    let size = monitor.size().to_logical::<f64>(scale);
    let position = monitor.position().to_logical::<f64>(scale);

    match app.get_webview_window(CANVAS_LABEL) {
        Some(w) => w.show()?,
        None => {
            let w = WebviewWindowBuilder::new(app, CANVAS_LABEL, WebviewUrl::App("index.html".into()))
                .title("Annotation canvas")
                .transparent(true)
                .decorations(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .shadow(false)
                .resizable(false)
                .focused(false)
                .position(position.x, position.y)
                .inner_size(size.width, size.height)
                .build()?;
            w.set_ignore_cursor_events(true)?;
        }
    }

    match app.get_webview_window(TOOLBAR_LABEL) {
        Some(w) => w.show()?,
        None => {
            let toolbar_x = position.x + TOOLBAR_MARGIN;
            let toolbar_y = position.y + ((size.height - TOOLBAR_HEIGHT) / 2.0).max(0.0);
            WebviewWindowBuilder::new(app, TOOLBAR_LABEL, WebviewUrl::App("index.html".into()))
                .title("Annotation toolbar")
                .transparent(false)
                .decorations(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .resizable(false)
                .position(toolbar_x, toolbar_y)
                .inner_size(TOOLBAR_WIDTH, TOOLBAR_HEIGHT)
                .build()?;
        }
    }

    Ok(())
}

/// Hides both overlay windows (without destroying them) and clears any
/// drawn shapes, so the next `show` starts from a blank canvas.
pub fn hide(app: &AppHandle) -> Result<()> {
    app.emit("annotation:clear", ())?;

    if let Some(w) = app.get_webview_window(CANVAS_LABEL) {
        w.hide()?;
    }
    if let Some(w) = app.get_webview_window(TOOLBAR_LABEL) {
        w.hide()?;
    }
    Ok(())
}
