import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

type RecordingStatus = "recording" | "paused" | "stopped";
type PipelineStatus = "recorded" | "transcribing" | "transcribed" | "failed";

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
}

interface StopRecordingResult {
  recordingId: string;
  videoPath: string;
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

const PIPELINE_LABEL: Record<PipelineStatus, string> = {
  recorded: "Awaiting transcription…",
  transcribing: "Transcribing…",
  transcribed: "Transcribed",
  failed: "Transcription failed",
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

  const [pipeline, setPipeline] = useState<RecordingSummary | null>(null);
  const [exportedPath, setExportedPath] = useState<string | null>(null);
  const recordingIdRef = useRef<string | null>(null);

  useEffect(() => {
    invoke<string[]>("list_microphones")
      .then((devices) => {
        setMicrophones(devices);
        if (devices.length > 0) setMicrophoneDevice(devices[0]);
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    const unlisten = listen<RecordingSummary>("recording-updated", (event) => {
      if (event.payload.id === recordingIdRef.current) {
        setPipeline(event.payload);
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

  async function handleStart() {
    setPipeline(null);
    setExportedPath(null);
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
      });
    }
  }

  async function handleExport(format: "srt" | "vtt") {
    if (!pipeline) return;
    const path = await run(() =>
      invoke<string>("export_subtitles", { recordingId: pipeline.id, format })
    );
    if (path) setExportedPath(path);
  }

  return (
    <main className="container">
      <h1>Screen Recorder — Capture Prototype</h1>
      <p className="subtitle">
        Week 2 milestone: batch transcription via faster-whisper (Whisper
        small, CUDA), SRT/VTT export.
      </p>

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
          <p className="output">
            <code>{pipeline.videoPath}</code>
          </p>
          {pipeline.status === "transcribed" && (
            <div className="controls">
              <button disabled={busy} onClick={() => handleExport("srt")}>
                Export .srt
              </button>
              <button disabled={busy} onClick={() => handleExport("vtt")}>
                Export .vtt
              </button>
            </div>
          )}
          {exportedPath && (
            <p className="output">
              Subtitles saved to: <code>{exportedPath}</code>
            </p>
          )}
        </section>
      )}
    </main>
  );
}

export default App;
