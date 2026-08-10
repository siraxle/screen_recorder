import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  DEFAULT_HOTKEY_SETTINGS,
  DEFAULT_LLM_SETTINGS,
  type HotkeySettings,
  type LlmSettingsResponse,
  type SttSettings,
} from "./types";
import DependencyPanel from "../setup/DependencyPanel";
import "../App.css";

type Tab = "speech" | "dependencies" | "hotkeys" | "llm";

const TABS: { id: Tab; label: string }[] = [
  { id: "speech", label: "Speech recognition" },
  { id: "dependencies", label: "Dependencies" },
  { id: "hotkeys", label: "Hotkeys" },
  { id: "llm", label: "LLM provider" },
];

function SettingsWindow() {
  const [tab, setTab] = useState<Tab>("speech");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [sttSettings, setSttSettings] = useState<SttSettings>({ engine: "whisper", device: "auto" });
  const [sttSettingsSaved, setSttSettingsSaved] = useState(false);

  const [hotkeySettings, setHotkeySettings] = useState<HotkeySettings>(DEFAULT_HOTKEY_SETTINGS);
  const [hotkeyInput, setHotkeyInput] = useState<string>(DEFAULT_HOTKEY_SETTINGS.toggleAnnotation);
  const [hotkeySettingsSaved, setHotkeySettingsSaved] = useState(false);
  const [hotkeyError, setHotkeyError] = useState<string | null>(null);

  const [llmSettings, setLlmSettings] = useState<LlmSettingsResponse>(DEFAULT_LLM_SETTINGS);
  const [cloudTokenInput, setCloudTokenInput] = useState<string>("");
  const [connectionStatus, setConnectionStatus] = useState<string | null>(null);
  const [settingsSaved, setSettingsSaved] = useState(false);

  useEffect(() => {
    invoke<SttSettings>("get_stt_settings")
      .then(setSttSettings)
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    invoke<HotkeySettings>("get_hotkey_settings")
      .then((s) => {
        setHotkeySettings(s);
        setHotkeyInput(s.toggleAnnotation);
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    invoke<LlmSettingsResponse>("get_llm_settings")
      .then(setLlmSettings)
      .catch((e) => setError(String(e)));
  }, []);

  async function run<T>(action: () => Promise<T>) {
    setBusy(true);
    setError(null);
    try {
      return await action();
    } catch (e) {
      setError(String(e));
      return undefined;
    } finally {
      setBusy(false);
    }
  }

  async function handleSaveSttSettings(next: SttSettings) {
    setSttSettingsSaved(false);
    setBusy(true);
    setError(null);
    try {
      await invoke("set_stt_settings", { settings: next });
      setSttSettings(next);
      setSttSettingsSaved(true);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handleSaveHotkeySettings() {
    setHotkeySettingsSaved(false);
    setHotkeyError(null);
    setBusy(true);
    try {
      const settings: HotkeySettings = { toggleAnnotation: hotkeyInput.trim() };
      await invoke("set_hotkey_settings", { settings });
      setHotkeySettings(settings);
      setHotkeySettingsSaved(true);
    } catch (e) {
      setHotkeyError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handleSaveLlmSettings() {
    setSettingsSaved(false);
    setBusy(true);
    setError(null);
    const { cloudTokenConfigured: _ignored, ...settings } = llmSettings;
    try {
      await invoke("set_llm_settings", { settings });
      setSettingsSaved(true);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handleCheckConnection() {
    setConnectionStatus("checking…");
    setBusy(true);
    try {
      await invoke("check_local_llm_connection", { baseUrl: llmSettings.localBaseUrl });
      setConnectionStatus("connected");
    } catch (e) {
      setConnectionStatus(`unreachable: ${String(e)}`);
    } finally {
      setBusy(false);
    }
  }

  async function handleSaveToken() {
    if (!cloudTokenInput.trim()) return;
    await run(() => invoke("set_cloud_api_token", { token: cloudTokenInput.trim() }));
    setCloudTokenInput("");
    setLlmSettings((prev) => ({ ...prev, cloudTokenConfigured: true }));
  }

  async function handleClearToken() {
    await run(() => invoke("clear_cloud_api_token"));
    setLlmSettings((prev) => ({ ...prev, cloudTokenConfigured: false }));
  }

  return (
    <main className="container settings-window">
      <h1>Settings</h1>
      <nav className="settings-tabs">
        {TABS.map((t) => (
          <button
            key={t.id}
            className={"settings-tab" + (tab === t.id ? " active" : "")}
            onClick={() => setTab(t.id)}
          >
            {t.label}
          </button>
        ))}
      </nav>
      {error && <p className="error">{error}</p>}

      {tab === "speech" && (
        <section className="panel">
          <h2>Speech recognition</h2>
          <label className="checkbox-row">
            <input
              type="radio"
              name="stt-engine"
              checked={sttSettings.engine === "whisper"}
              onChange={() => handleSaveSttSettings({ ...sttSettings, engine: "whisper" })}
            />
            Whisper (faster-whisper) — 99+ languages, auto-detects the spoken language
          </label>
          <label className="checkbox-row">
            <input
              type="radio"
              name="stt-engine"
              checked={sttSettings.engine === "parakeet"}
              onChange={() => handleSaveSttSettings({ ...sttSettings, engine: "parakeet" })}
            />
            NVIDIA Parakeet-TDT-0.6B-v3 — ~25 languages, much faster, no language auto-detect
          </label>

          <h2>Compute device</h2>
          <label className="checkbox-row">
            <input
              type="radio"
              name="stt-device"
              checked={sttSettings.device === "auto"}
              onChange={() => handleSaveSttSettings({ ...sttSettings, device: "auto" })}
            />
            Auto — use the GPU when an NVIDIA card and its packages are installed, CPU otherwise
          </label>
          <label className="checkbox-row">
            <input
              type="radio"
              name="stt-device"
              checked={sttSettings.device === "cuda"}
              onChange={() => handleSaveSttSettings({ ...sttSettings, device: "cuda" })}
            />
            Force GPU (NVIDIA) — requires the GPU packages installed on the Dependencies tab
          </label>
          <label className="checkbox-row">
            <input
              type="radio"
              name="stt-device"
              checked={sttSettings.device === "cpu"}
              onChange={() => handleSaveSttSettings({ ...sttSettings, device: "cpu" })}
            />
            Force CPU
          </label>
          {sttSettingsSaved && <span className="output"> Saved.</span>}
        </section>
      )}

      {tab === "dependencies" && (
        <section className="panel">
          <h2>Dependencies</h2>
          <DependencyPanel />
        </section>
      )}

      {tab === "hotkeys" && (
        <section className="panel">
          <h2>Hotkeys</h2>
          <label className="checkbox-row">
            Show/hide annotation toolbar:{" "}
            <input
              type="text"
              value={hotkeyInput}
              onChange={(e) => {
                setHotkeyInput(e.target.value);
                setHotkeySettingsSaved(false);
              }}
              placeholder="CommandOrControl+Shift+A"
            />
          </label>
          <p className="hint">
            Works from anywhere, even while another app is focused. Format example:
            CommandOrControl+Shift+A (Ctrl on Windows/Linux, Cmd on macOS).
          </p>
          <button
            disabled={busy || hotkeyInput.trim() === hotkeySettings.toggleAnnotation}
            onClick={handleSaveHotkeySettings}
          >
            Save
          </button>
          {hotkeySettingsSaved && <span className="output"> Saved.</span>}
          {hotkeyError && <span className="error"> {hotkeyError}</span>}
        </section>
      )}

      {tab === "llm" && (
        <section className="panel">
          <h2>LLM provider</h2>
          <label className="checkbox-row">
            <input
              type="radio"
              name="llm-provider"
              checked={llmSettings.provider === "local"}
              onChange={() => setLlmSettings((prev) => ({ ...prev, provider: "local" }))}
            />
            Local (Ollama / LM Studio / llama.cpp server / vLLM)
          </label>
          {llmSettings.provider === "local" && (
            <>
              <input
                type="text"
                placeholder="Base URL, e.g. http://localhost:11434/v1"
                value={llmSettings.localBaseUrl}
                onChange={(e) => {
                  const value = e.currentTarget.value;
                  setLlmSettings((prev) => ({ ...prev, localBaseUrl: value }));
                }}
              />
              <input
                type="text"
                placeholder="Model, e.g. llama3.1"
                value={llmSettings.localModel}
                onChange={(e) => {
                  const value = e.currentTarget.value;
                  setLlmSettings((prev) => ({ ...prev, localModel: value }));
                }}
              />
              <button disabled={busy} onClick={handleCheckConnection}>
                Check connection
              </button>
              {connectionStatus && <span className="output"> {connectionStatus}</span>}
            </>
          )}

          <label className="checkbox-row">
            <input
              type="radio"
              name="llm-provider"
              checked={llmSettings.provider === "cloud"}
              onChange={() => setLlmSettings((prev) => ({ ...prev, provider: "cloud" }))}
            />
            Cloud (OpenAI-compatible endpoint + API token)
          </label>
          {llmSettings.provider === "cloud" && (
            <>
              <input
                type="text"
                placeholder="Base URL, e.g. https://api.openai.com/v1"
                value={llmSettings.cloudBaseUrl}
                onChange={(e) => {
                  const value = e.currentTarget.value;
                  setLlmSettings((prev) => ({ ...prev, cloudBaseUrl: value }));
                }}
              />
              <input
                type="text"
                placeholder="Model, e.g. gpt-4o-mini"
                value={llmSettings.cloudModel}
                onChange={(e) => {
                  const value = e.currentTarget.value;
                  setLlmSettings((prev) => ({ ...prev, cloudModel: value }));
                }}
              />
              <label className="checkbox-row">
                API token
                <input
                  type="password"
                  placeholder={llmSettings.cloudTokenConfigured ? "•••• saved ••••" : "not set"}
                  value={cloudTokenInput}
                  onChange={(e) => setCloudTokenInput(e.currentTarget.value)}
                />
              </label>
              <div className="controls">
                <button disabled={busy || !cloudTokenInput.trim()} onClick={handleSaveToken}>
                  Save token
                </button>
                <button disabled={busy || !llmSettings.cloudTokenConfigured} onClick={handleClearToken}>
                  Clear token
                </button>
              </div>
            </>
          )}

          <button disabled={busy} onClick={handleSaveLlmSettings}>
            Save settings
          </button>
          {settingsSaved && <span className="output"> Saved.</span>}
        </section>
      )}
    </main>
  );
}

export default SettingsWindow;
