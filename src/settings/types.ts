export type ProviderKind = "local" | "cloud";
export type SttEngine = "whisper" | "parakeet";
export type SttDevice = "auto" | "cuda" | "cpu";

export interface SttSettings {
  engine: SttEngine;
  device: SttDevice;
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

// Mirrors src-tauri/src/setup/mod.rs's DependencyId/DependencyStatus/
// DependencyReport/InstallProgress (serde camelCase on the struct fields,
// snake_case on the DependencyId/DependencySource enum values).
export type DependencyId = "python" | "ffmpeg" | "gpu_packages";

// Mirrors src-tauri/src/setup/overrides.rs's DependencySource.
export type DependencySource = "bundled" | "system" | "custom";

export interface DependencyStatus {
  id: DependencyId;
  satisfied: boolean;
  /** `gpu_packages` only: false on a machine with no NVIDIA GPU -- CPU always works, so this row never blocks. */
  applicable: boolean;
  detail: string;
  approxDownloadBytes: number;
  source: DependencySource;
  /** Set when `source !== "bundled"`: the folder the system-detected/custom install lives in. */
  overridePath: string | null;
}

// Mirrors src-tauri/src/setup/scan.rs's ScannedCandidate.
export interface ScannedCandidate {
  id: DependencyId;
  path: string;
  detail: string;
}

export interface DependencyReport {
  gpuPresent: boolean;
  statuses: DependencyStatus[];
}

/** Mirrors `DependencyReport::required_satisfied` on the Rust side -- GpuPackages never gates the app. */
export function requiredDependenciesSatisfied(report: DependencyReport): boolean {
  return report.statuses.every((s) => s.id === "gpu_packages" || s.satisfied);
}

export type InstallStage = "downloading" | "extracting" | "installing" | "done";

export interface InstallProgress {
  dependency: DependencyId;
  stage: InstallStage;
  progress: number;
}

export const EVENT_DEPENDENCY_INSTALL_PROGRESS = "dependency-install-progress";
export const EVENT_DEPENDENCIES_CHANGED = "dependencies-changed";

export const DEPENDENCY_LABELS: Record<DependencyId, string> = {
  python: "Python runtime",
  ffmpeg: "FFmpeg",
  gpu_packages: "GPU acceleration (NVIDIA)",
};
