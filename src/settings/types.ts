export type ProviderKind = "local" | "cloud";
export type SttEngine = "whisper" | "parakeet";

export interface SttSettings {
  engine: SttEngine;
}

export interface HotkeySettings {
  toggleAnnotation: string;
}

export interface LlmSettings {
  provider: ProviderKind;
  localBaseUrl: string;
  localModel: string;
  cloudBaseUrl: string;
  cloudModel: string;
}

export interface LlmSettingsResponse extends LlmSettings {
  cloudTokenConfigured: boolean;
}

export const DEFAULT_LLM_SETTINGS: LlmSettingsResponse = {
  provider: "local",
  localBaseUrl: "",
  localModel: "",
  cloudBaseUrl: "",
  cloudModel: "",
  cloudTokenConfigured: false,
};

// Mirrors src-tauri/src/hotkeys.rs::DEFAULT_TOGGLE_ANNOTATION — used only as
// the initial state before `get_hotkey_settings` resolves.
export const DEFAULT_HOTKEY_SETTINGS: HotkeySettings = {
  toggleAnnotation: "CommandOrControl+Shift+A",
};

// Emitted by the Rust `set_stt_settings` command (see commands.rs) so the
// main recorder window's cached engine choice — used for the recording
// language picker — stays in sync when the engine is changed from the
// separate Settings window.
export const EVENT_STT_SETTINGS_CHANGED = "stt-settings-changed";
