import { useEffect, useRef, useState, type CSSProperties } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

type RecordingStatus = "recording" | "paused" | "stopped";
type PipelineStatus =
  | "recorded"
  | "transcribing"
  | "transcribed"
  | "summarizing"
  | "summarized"
  | "failed";

type ProviderKind = "local" | "cloud";

interface LlmSettings {
  provider: ProviderKind;
  localBaseUrl: string;
  localModel: string;
  cloudBaseUrl: string;
  cloudModel: string;
}

interface LlmSettingsResponse extends LlmSettings {
  cloudTokenConfigured: boolean;
}

interface Summary {
  overview: string;
  keyPoints: string[];
  language: string;
}

interface CaptureConfig {
  captureSystemAudio: boolean;
  captureMicrophone: boolean;
  microphoneDevice: string | null;
  framerate: number;
}

interface RecordingSummary {
  id: string;
  videoPath: string;
  createdAt: string;
  status: PipelineStatus;
  detectedLanguage: string | null;
  durationMs: number | null;
}

interface StopRecordingResult {
  recordingId: string;
  videoPath: string;
}

interface TranscriptionProgress {
  recordingId: string;
  progress: number; // 0.0..=1.0
}

interface JobFailed {
  recordingId: string;
  stage: string;
  message: string;
}

interface TranslationUpdate {
  recordingId: string;
  language: string;
  status: "done" | "failed";
  failedSegments: number;
  totalSegments: number;
  message: string | null;
}

interface ExportSubtitlesResult {
  paths: string[];
}

const LANGUAGE_OPTIONS: Array<{ code: string; label: string }> = [
  { code: "", label: "Auto-detect" },
  { code: "en", label: "English" },
  { code: "ru", label: "Russian" },
  { code: "es", label: "Spanish" },
  { code: "de", label: "German" },
  { code: "fr", label: "French" },
  { code: "other", label: "Other (enter code)" },
];

// F8: subtitle language defaults to "same as video" (no translation), unlike
// the transcript-language picker's "auto-detect" default — the two settings
// are deliberately distinct (CLAUDE.md: "never conflate them").
const SUBTITLE_LANGUAGE_OPTIONS: Array<{ code: string; label: string }> = [
  { code: "", label: "Same as video (no translation)" },
  ...LANGUAGE_OPTIONS.slice(1),
];

const PIPELINE_LABEL: Record<PipelineStatus, string> = {
  recorded: "Awaiting transcription…",
  transcribing: "Transcribing…",
  transcribed: "Transcribed",
  summarizing: "Generating summary…",
  summarized: "Summarized",
  failed: "Processing failed",
};

function formatDuration(durationMs: number | null): string {
  if (durationMs == null) return "—";
  const totalSeconds = Math.round(durationMs / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  const pad = (n: number) => n.toString().padStart(2, "0");
  return hours > 0
    ? `${hours}:${pad(minutes)}:${pad(seconds)}`
    : `${minutes}:${pad(seconds)}`;
}

function formatCreatedAt(createdAt: string): string {
  if (!createdAt) return "—";
  const date = new Date(createdAt);
  if (Number.isNaN(date.getTime())) return createdAt;
  return date.toLocaleString();
}

const DEFAULT_LLM_SETTINGS: LlmSettingsResponse = {
  provider: "local",
  localBaseUrl: "",
  localModel: "",
  cloudBaseUrl: "",
  cloudModel: "",
  cloudTokenConfigured: false,
};

function App() {
  const [status, setStatus] = useState<RecordingStatus>("stopped");
  const [microphones, setMicrophones] = useState<string[]>([]);
  const [captureSystemAudio, setCaptureSystemAudio] = useState(true);
  const [captureMicrophone, setCaptureMicrophone] = useState(true);
  const [microphoneDevice, setMicrophoneDevice] = useState<string>("");
  const [languageOverride, setLanguageOverride] = useState<string>("");
  const [customLanguage, setCustomLanguage] = useState<string>("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [recordings, setRecordings] = useState<RecordingSummary[]>([]);
  const [pipeline, setPipeline] = useState<RecordingSummary | null>(null);
  const [transcriptionProgress, setTranscriptionProgress] = useState<number | null>(null);
  const [exportedPaths, setExportedPaths] = useState<string[] | null>(null);
  const [recordingsDir, setRecordingsDir] = useState<string>("");
  const recordingIdRef = useRef<string | null>(null);

  const [llmSettings, setLlmSettings] = useState<LlmSettingsResponse>(DEFAULT_LLM_SETTINGS);
  const [cloudTokenInput, setCloudTokenInput] = useState<string>("");
  const [connectionStatus, setConnectionStatus] = useState<string | null>(null);
  const [settingsSaved, setSettingsSaved] = useState(false);

  const [summaryLanguage, setSummaryLanguage] = useState<string>("");
  const [customSummaryLanguage, setCustomSummaryLanguage] = useState<string>("");
  const [summary, setSummary] = useState<Summary | null>(null);

  const [subtitleLanguage, setSubtitleLanguage] = useState<string>("");
  const [customSubtitleLanguage, setCustomSubtitleLanguage] = useState<string>("");
  const [dualSubtitles, setDualSubtitles] = useState(false);
  const [separateSubtitleFiles, setSeparateSubtitleFiles] = useState(false);
  const [translating, setTranslating] = useState(false);
  const [translationInfo, setTranslationInfo] = useState<string | null>(null);
  const [translatedLanguages, setTranslatedLanguages] = useState<Set<string>>(new Set());

  useEffect(() => {
    invoke<string[]>("list_microphones")
      .then((devices) => {
        setMicrophones(devices);
        if (devices.length > 0) setMicrophoneDevice(devices[0]);
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    invoke<string>("get_recordings_dir")
      .then(setRecordingsDir)
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    invoke<LlmSettingsResponse>("get_llm_settings")
      .then(setLlmSettings)
      .catch((e) => setError(String(e)));
  }, []);

  function refreshRecordings() {
    invoke<RecordingSummary[]>("list_recordings")
      .then(setRecordings)
      .catch((e) => setError(String(e)));
  }

  useEffect(() => {
    refreshRecordings();
  }, []);

  useEffect(() => {
    if (pipeline?.status !== "summarized" || !recordingIdRef.current) return;
    invoke<Summary | null>("get_summary", { recordingId: recordingIdRef.current })
      .then(setSummary)
      .catch((e) => setError(String(e)));
  }, [pipeline?.status]);

  useEffect(() => {
    const unlisten = listen<RecordingSummary>("recording-updated", (event) => {
      if (event.payload.id === recordingIdRef.current) {
        setPipeline(event.payload);
        setTranscriptionProgress(event.payload.status === "transcribing" ? 0 : null);
      }
      setRecordings((prev) => {
        const next = prev.filter((r) => r.id !== event.payload.id);
        next.unshift(event.payload);
        next.sort((a, b) => b.createdAt.localeCompare(a.createdAt));
        return next;
      });
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  useEffect(() => {
    const unlisten = listen<TranscriptionProgress>("transcription-progress", (event) => {
      if (event.payload.recordingId === recordingIdRef.current) {
        setTranscriptionProgress(event.payload.progress);
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  useEffect(() => {
    const unlisten = listen<JobFailed>("job-failed", (event) => {
      if (event.payload.recordingId === recordingIdRef.current) {
        setError(`${event.payload.stage} failed: ${event.payload.message}`);
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  useEffect(() => {
    const unlisten = listen<TranslationUpdate>("translation-updated", (event) => {
      if (event.payload.recordingId !== recordingIdRef.current) return;
      setTranslating(false);
      if (event.payload.status === "done") {
        setTranslatedLanguages((prev) => new Set(prev).add(event.payload.language));
        setTranslationInfo(
          event.payload.failedSegments > 0
            ? `Translated to ${event.payload.language} — ${event.payload.failedSegments} of ${event.payload.totalSegments} segments kept the original text (translation failed for those).`
            : `Translated to ${event.payload.language}.`
        );
      } else {
        setTranslationInfo(`Translation failed: ${event.payload.message ?? "unknown error"}`);
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
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

  function resolveLanguageOverride(): string | null {
    if (languageOverride === "") return null;
    if (languageOverride === "other") return customLanguage.trim() || null;
    return languageOverride;
  }

  function resolveSummaryLanguage(): string | null {
    if (summaryLanguage === "") return null;
    if (summaryLanguage === "other") return customSummaryLanguage.trim() || null;
    return summaryLanguage;
  }

  function resolveSubtitleLanguage(): string | null {
    if (subtitleLanguage === "") return null;
    if (subtitleLanguage === "other") return customSubtitleLanguage.trim() || null;
    return subtitleLanguage;
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

  async function handleGenerateSummary() {
    if (!pipeline) return;
    setSummary(null);
    await run(() =>
      invoke("generate_summary", {
        recordingId: pipeline.id,
        summaryLanguage: resolveSummaryLanguage(),
      })
    );
  }

  async function handleTranslate() {
    if (!pipeline) return;
    const targetLanguage = resolveSubtitleLanguage();
    if (!targetLanguage) return;
    setTranslating(true);
    setTranslationInfo(null);
    try {
      await invoke("translate_subtitles", { recordingId: pipeline.id, targetLanguage });
    } catch (e) {
      setTranslating(false);
      setError(String(e));
    }
  }

  async function handleStart() {
    setPipeline(null);
    setTranscriptionProgress(null);
    setExportedPaths(null);
    setSummary(null);
    setTranslatedLanguages(new Set());
    setTranslationInfo(null);
    recordingIdRef.current = null;
    const config: CaptureConfig = {
      captureSystemAudio,
      captureMicrophone,
      microphoneDevice: captureMicrophone && microphoneDevice ? microphoneDevice : null,
      framerate: 30,
    };
    await run(() =>
      invoke("start_recording", {
        settings: { capture: config, languageOverride: resolveLanguageOverride() },
      })
    );
    setStatus("recording");
  }

  async function handlePause() {
    await run(() => invoke("pause_recording"));
    setStatus("paused");
  }

  async function handleResume() {
    await run(() => invoke("resume_recording"));
    setStatus("recording");
  }

  async function handleStop() {
    const result = await run(() => invoke<StopRecordingResult>("stop_recording"));
    setStatus("stopped");
    if (result) {
      recordingIdRef.current = result.recordingId;
      setPipeline({
        id: result.recordingId,
        videoPath: result.videoPath,
        createdAt: "",
        status: "recorded",
        detectedLanguage: null,
        durationMs: null,
      });
      setTranscriptionProgress(null);
      refreshRecordings();
    }
  }

  function selectRecording(recording: RecordingSummary) {
    recordingIdRef.current = recording.id;
    setPipeline(recording);
    setTranscriptionProgress(null);
    setExportedPaths(null);
    setSummary(null);
    setTranslatedLanguages(new Set());
    setTranslationInfo(null);
  }

  async function handleChangeRecordingsDir() {
    const chosen = await run(() => invoke<string | null>("pick_recordings_dir"));
    if (!chosen) return;
    await run(() => invoke("set_recordings_dir", { path: chosen }));
    setRecordingsDir(chosen);
  }

  async function handleExport(format: "srt" | "vtt") {
    if (!pipeline) return;
    const language = resolveSubtitleLanguage();
    const mode = language === null ? "original" : dualSubtitles ? "dual" : "translated";
    const result = await run(() =>
      invoke<ExportSubtitlesResult>("export_subtitles", {
        recordingId: pipeline.id,
        format,
        mode,
        language,
        separateFiles: mode === "dual" ? separateSubtitleFiles : false,
      })
    );
    if (result) setExportedPaths(result.paths);
  }

  return (
    <main className="container">
      <h1>Screen Recorder — Capture Prototype</h1>
      <p className="subtitle">
        Week 3 milestone: LLM provider settings, transcript summaries.
      </p>

      <section className="panel">
        <label className="checkbox-row path-row">
          Save recordings to
          <code className="path-value">{recordingsDir || "…"}</code>
        </label>
        <button disabled={busy || status !== "stopped"} onClick={handleChangeRecordingsDir}>
          Change folder…
        </button>
      </section>

      <section className="panel">
        <h2>Recordings library</h2>
        {recordings.length === 0 && <p className="output">No recordings yet.</p>}
        {recordings.length > 0 && (
          <table className="recordings-table">
            <thead>
              <tr>
                <th>Date</th>
                <th>Duration</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody>
              {recordings.map((recording) => (
                <tr
                  key={recording.id}
                  className={recording.id === pipeline?.id ? "selected" : undefined}
                  onClick={() => selectRecording(recording)}
                >
                  <td>{formatCreatedAt(recording.createdAt)}</td>
                  <td>{formatDuration(recording.durationMs)}</td>
                  <td>
                    {PIPELINE_LABEL[recording.status]}
                    {recording.detectedLanguage && ` (${recording.detectedLanguage})`}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

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

      <section className="panel">
        <label className="checkbox-row">
          <input
            type="checkbox"
            checked={captureMicrophone}
            disabled={status !== "stopped"}
            onChange={(e) => setCaptureMicrophone(e.currentTarget.checked)}
          />
          Microphone
        </label>
        {captureMicrophone && (
          <select
            value={microphoneDevice}
            disabled={status !== "stopped"}
            onChange={(e) => setMicrophoneDevice(e.currentTarget.value)}
          >
            {microphones.length === 0 && <option value="">No microphone found</option>}
            {microphones.map((device) => (
              <option key={device} value={device}>
                {device}
              </option>
            ))}
          </select>
        )}

        <label className="checkbox-row">
          <input
            type="checkbox"
            checked={captureSystemAudio}
            disabled={status !== "stopped"}
            onChange={(e) => setCaptureSystemAudio(e.currentTarget.checked)}
          />
          System audio (WASAPI loopback)
        </label>

        <label className="checkbox-row">
          Transcript language
          <select
            value={languageOverride}
            disabled={status !== "stopped"}
            onChange={(e) => setLanguageOverride(e.currentTarget.value)}
          >
            {LANGUAGE_OPTIONS.map((option) => (
              <option key={option.code} value={option.code}>
                {option.label}
              </option>
            ))}
          </select>
        </label>
        {languageOverride === "other" && (
          <input
            type="text"
            placeholder="ISO 639-1 code, e.g. pt"
            value={customLanguage}
            disabled={status !== "stopped"}
            onChange={(e) => setCustomLanguage(e.currentTarget.value)}
          />
        )}
      </section>

      <section className="controls">
        {status === "stopped" && (
          <button disabled={busy} onClick={handleStart}>
            Start Recording
          </button>
        )}
        {status === "recording" && (
          <>
            <button disabled={busy} onClick={handlePause}>
              Pause
            </button>
            <button disabled={busy} onClick={handleStop}>
              Stop
            </button>
          </>
        )}
        {status === "paused" && (
          <>
            <button disabled={busy} onClick={handleResume}>
              Resume
            </button>
            <button disabled={busy} onClick={handleStop}>
              Stop
            </button>
          </>
        )}
      </section>

      <p className="status">
        Status: <strong>{status}</strong>
      </p>

      {error && <p className="error">Error: {error}</p>}

      {pipeline && (
        <section className="panel">
          <p className="status">
            Transcription: <strong>{PIPELINE_LABEL[pipeline.status]}</strong>
            {pipeline.detectedLanguage && ` (${pipeline.detectedLanguage})`}
          </p>
          {pipeline.status === "transcribing" && (
            <div className="progress-bar">
              <div
                className="progress-bar-fill"
                role="progressbar"
                aria-valuenow={Math.round((transcriptionProgress ?? 0) * 100)}
                aria-valuemin={0}
                aria-valuemax={100}
                style={{ "--progress": `${Math.round((transcriptionProgress ?? 0) * 100)}%` } as CSSProperties}
              />
              <span className="progress-bar-label">
                {Math.round((transcriptionProgress ?? 0) * 100)}%
              </span>
            </div>
          )}
          <p className="output">
            <code>{pipeline.videoPath}</code>
          </p>
          {(pipeline.status === "transcribed" ||
            pipeline.status === "summarizing" ||
            pipeline.status === "summarized") && (
            <div className="panel">
              <label className="checkbox-row">
                Subtitle language
                <select
                  value={subtitleLanguage}
                  onChange={(e) => setSubtitleLanguage(e.currentTarget.value)}
                >
                  {SUBTITLE_LANGUAGE_OPTIONS.map((option) => (
                    <option key={option.code} value={option.code}>
                      {option.label}
                    </option>
                  ))}
                </select>
              </label>
              {subtitleLanguage === "other" && (
                <input
                  type="text"
                  placeholder="ISO 639-1 code, e.g. pt"
                  value={customSubtitleLanguage}
                  onChange={(e) => setCustomSubtitleLanguage(e.currentTarget.value)}
                />
              )}

              {resolveSubtitleLanguage() !== null && (
                <>
                  <label className="checkbox-row">
                    <input
                      type="checkbox"
                      checked={dualSubtitles}
                      onChange={(e) => setDualSubtitles(e.currentTarget.checked)}
                    />
                    Dual subtitles (original + translation)
                  </label>
                  {dualSubtitles && (
                    <label className="checkbox-row">
                      <input
                        type="checkbox"
                        checked={separateSubtitleFiles}
                        onChange={(e) => setSeparateSubtitleFiles(e.currentTarget.checked)}
                      />
                      Export as separate files (original.&lt;lang&gt; + translated.&lt;lang&gt;)
                    </label>
                  )}
                  <button disabled={busy || translating} onClick={handleTranslate}>
                    {translating ? "Translating…" : "Translate"}
                  </button>
                  {translationInfo && <p className="output">{translationInfo}</p>}
                </>
              )}

              <div className="controls">
                <button
                  disabled={
                    busy ||
                    (resolveSubtitleLanguage() !== null &&
                      !translatedLanguages.has(resolveSubtitleLanguage() as string))
                  }
                  onClick={() => handleExport("srt")}
                >
                  Export .srt
                </button>
                <button
                  disabled={
                    busy ||
                    (resolveSubtitleLanguage() !== null &&
                      !translatedLanguages.has(resolveSubtitleLanguage() as string))
                  }
                  onClick={() => handleExport("vtt")}
                >
                  Export .vtt
                </button>
              </div>
              {resolveSubtitleLanguage() !== null &&
                !translatedLanguages.has(resolveSubtitleLanguage() as string) && (
                  <p className="output">Run Translate before exporting.</p>
                )}
            </div>
          )}
          {exportedPaths && exportedPaths.length > 0 && (
            <p className="output">
              Subtitles saved to:
              {exportedPaths.map((p) => (
                <code key={p} style={{ display: "block" }}>
                  {p}
                </code>
              ))}
            </p>
          )}

          {(pipeline.status === "transcribed" ||
            pipeline.status === "summarizing" ||
            pipeline.status === "summarized") && (
            <div className="panel">
              <label className="checkbox-row">
                Summary language
                <select
                  value={summaryLanguage}
                  onChange={(e) => setSummaryLanguage(e.currentTarget.value)}
                >
                  <option value="">Same as transcript</option>
                  {LANGUAGE_OPTIONS.filter((o) => o.code !== "").map((option) => (
                    <option key={option.code} value={option.code}>
                      {option.label}
                    </option>
                  ))}
                </select>
              </label>
              {summaryLanguage === "other" && (
                <input
                  type="text"
                  placeholder="ISO 639-1 code, e.g. pt"
                  value={customSummaryLanguage}
                  onChange={(e) => setCustomSummaryLanguage(e.currentTarget.value)}
                />
              )}
              <button
                disabled={busy || pipeline.status === "summarizing"}
                onClick={handleGenerateSummary}
              >
                {pipeline.status === "summarizing" ? "Generating…" : "Generate summary"}
              </button>

              {summary && (
                <div className="output">
                  <p>{summary.overview}</p>
                  {summary.keyPoints.length > 0 && (
                    <ul>
                      {summary.keyPoints.map((point, i) => (
                        <li key={i}>{point}</li>
                      ))}
                    </ul>
                  )}
                </div>
              )}
            </div>
          )}
        </section>
      )}
    </main>
  );
}

export default App;
