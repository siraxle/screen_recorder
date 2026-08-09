use std::str::FromStr;

use anyhow::{anyhow, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

use crate::db;
use crate::state::AppState;

/// The OS-wide hotkey that opens/closes the annotation overlay. It must work
/// while any other app is focused — that's the whole point of an on-screen
/// annotation tool — so it's registered with the OS via
/// `tauri-plugin-global-shortcut`, not just handled inside our own windows.
pub const DEFAULT_TOGGLE_ANNOTATION: &str = "CommandOrControl+Shift+A";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeySettings {
    pub toggle_annotation: String,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self { toggle_annotation: DEFAULT_TOGGLE_ANNOTATION.to_string() }
    }
}

const KEY_TOGGLE_ANNOTATION: &str = "hotkey_toggle_annotation";

/// Mirrors `pipeline::stt::load_settings`'s fallback-to-default convention.
pub fn load_settings(conn: &Connection) -> Result<HotkeySettings> {
    let toggle_annotation = db::get_setting(conn, KEY_TOGGLE_ANNOTATION)?
        .unwrap_or_else(|| DEFAULT_TOGGLE_ANNOTATION.to_string());
    Ok(HotkeySettings { toggle_annotation })
}

pub fn save_settings(conn: &Connection, settings: &HotkeySettings) -> Result<()> {
    db::set_setting(conn, KEY_TOGGLE_ANNOTATION, &settings.toggle_annotation)
}

/// Parses `accelerator` and (re)registers it as the global annotation-toggle
/// hotkey, unregistering whatever was previously registered first so
/// changing the setting never leaves a stale OS-level binding behind.
pub async fn apply(app: &AppHandle, state: &AppState, accelerator: &str) -> Result<()> {
    let shortcut = Shortcut::from_str(accelerator)
        .map_err(|_| anyhow!("\"{accelerator}\" is not a valid hotkey (e.g. \"CommandOrControl+Shift+A\")"))?;

    let mut guard = state.active_hotkey.lock().await;
    if let Some(old) = guard.take() {
        // Best-effort: if it was never actually registered (e.g. app just
        // started), unregistering is a harmless no-op.
        let _ = app.global_shortcut().unregister(old);
    }
    app.global_shortcut()
        .register(shortcut)
        .map_err(|e| anyhow!("could not register \"{accelerator}\" (likely already used by another app): {e}"))?;
    *guard = Some(shortcut);
    Ok(())
}
