import { useEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { EVENT_CLOSED, EVENT_OPENED } from "./annotation/types";
import { EVENT_STT_SETTINGS_CHANGED, type SttSettings } from "./settings/types";
import "./App.css";

type RecordingStatus = "recording" | "paused" | "stopped";
type PipelineStatus =
  | "recorded"
  | "transcribing"
  | "transcribed"
  | "summarizing"
  | "summarized"
  | "failed";

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
  title: string | null;
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

interface ThermalCooldown {
  recordingId: string;
  gpuTempC: number;
}

interface TranslationProgress {
  recordingId: string;
  progress: number; // 0.0..=1.0
}

interface TranslationUpdate {
  recordingId: string;
  language: string;
  status: "done" | "failed" | "cancelled";
  failedSegments: number;
  totalSegments: number;
  message: string | null;
}

interface ExportSubtitlesResult {
  paths: string[];
}

interface TranslationStateResult {
  languages: string[];
  inProgress: boolean;
}

interface SearchMatch {
  source: "transcript" | "summary" | "title";
  snippet: string;
  startMs: number | null;
}

interface SearchResult {
  recording: RecordingSummary;
  matches: SearchMatch[];
}

interface LiveSubtitlesSettings {
  enabled: boolean;
}

interface LiveSubtitleCaption {
  text: string;
}

interface LiveSubtitleCooldown {
  gpuTempC: number;
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

// Parakeet-TDT-0.6B-v3 takes no language hint and never auto-detects (unlike
// Whisper) — see pipeline/stt.rs's parakeet_command doc comment — so there's
// no "Auto-detect" entry here, and the list is capped to the model's actual
// 25 trained languages rather than offering a free-text "Other" that would
// silently produce poor results.
const PARAKEET_LANGUAGE_OPTIONS: Array<{ code: string; label: string }> = [
  { code: "en", label: "English" },
  { code: "ru", label: "Russian" },
  { code: "uk", label: "Ukrainian" },
  { code: "es", label: "Spanish" },
  { code: "de", label: "German" },
  { code: "fr", label: "French" },
  { code: "it", label: "Italian" },
  { code: "pt", label: "Portuguese" },
  { code: "nl", label: "Dutch" },
  { code: "pl", label: "Polish" },
  { code: "cs", label: "Czech" },
  { code: "sk", label: "Slovak" },
  { code: "sl", label: "Slovenian" },
  { code: "hr", label: "Croatian" },
  { code: "hu", label: "Hungarian" },
  { code: "ro", label: "Romanian" },
  { code: "bg", label: "Bulgarian" },
  { code: "el", label: "Greek" },
  { code: "da", label: "Danish" },
  { code: "sv", label: "Swedish" },
  { code: "fi", label: "Finnish" },
  { code: "et", label: "Estonian" },
  { code: "lv", label: "Latvian" },
  { code: "lt", label: "Lithuanian" },
  { code: "mt", label: "Maltese" },
];
const PARAKEET_LANGUAGE_CODES = new Set(PARAKEET_LANGUAGE_OPTIONS.map((o) => o.code));

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

function formatTimecode(ms: number): string {
  const totalSeconds = Math.floor(ms / 1000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

const SNIPPET_MARK_START = "\u0001";
const SNIPPET_MARK_END = "\u0002";

// Splits a search snippet on the backend's highlight markers into plain
// text and <mark> spans, rather than trusting dangerouslySetInnerHTML with
// transcript/summary text the app doesn't control.
function renderSnippet(snippet: string): ReactNode[] {
  const parts = snippet.split(SNIPPET_MARK_START);
  const nodes: ReactNode[] = [parts[0]];
  for (let i = 1; i < parts.length; i++) {
    const [marked, ...rest] = parts[i].split(SNIPPET_MARK_END);
    nodes.push(<mark key={i}>{marked}</mark>);
    nodes.push(rest.join(SNIPPET_MARK_END));
  }
  return nodes;
}

function formatCreatedAt(createdAt: string): string {
  if (!createdAt) return "—";
  const date = new Date(createdAt);
  if (Number.isNaN(date.getTime())) return createdAt;
  return date.toLocaleString();
}

function displayName(recording: RecordingSummary): string {
  return recording.title || formatCreatedAt(recording.createdAt);
}

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
  const [annotationOverlayVisible, setAnnotationOverlayVisible] = useState(false);

  const [recordings, setRecordings] = useState<RecordingSummary[]>([]);
  const [pipeline, setPipeline] = useState<RecordingSummary | null>(null);
  const [editingTitleId, setEditingTitleId] = useState<string | null>(null);
  const [editingTitle, setEditingTitle] = useState<string>("");
  const [renaming, setRenaming] = useState(false);
  const [transcriptionProgress, setTranscriptionProgress] = useState<number | null>(null);
  const [exportedPaths, setExportedPaths] = useState<string[] | null>(null);
  const [recordingsDir, setRecordingsDir] = useState<string>("");
  const recordingIdRef = useRef<string | null>(null);

  const [searchQuery, setSearchQuery] = useState<string>("");
  const [searchResults, setSearchResults] = useState<SearchResult[] | null>(null);
  const [searching, setSearching] = useState(false);

  const [sttSettings, setSttSettings] = useState<SttSettings>({ engine: "whisper" });

  const [liveSubtitlesEnabled, setLiveSubtitlesEnabled] = useState(false);
  const [liveCaption, setLiveCaption] = useState<string>("");
  const [liveSubtitlesCooldown, setLiveSubtitlesCooldown] = useState(false);

  const [summaryLanguage, setSummaryLanguage] = useState<string>("");
  const [customSummaryLanguage, setCustomSummaryLanguage] = useState<string>("");
  const [summary, setSummary] = useState<Summary | null>(null);

  const [subtitleLanguage, setSubtitleLanguage] = useState<string>("");
  const [customSubtitleLanguage, setCustomSubtitleLanguage] = useState<string>("");
  const [dualSubtitles, setDualSubtitles] = useState(false);
  const [separateSubtitleFiles, setSeparateSubtitleFiles] = useState(false);
  const [translating, setTranslating] = useState(false);
  const [translationPaused, setTranslationPaused] = useState(false);
  const [translationProgress, setTranslationProgress] = useState<number | null>(null);
  const [translationInfo, setTranslationInfo] = useState<string | null>(null);
  const [translatedLanguages, setTranslatedLanguages] = useState<Set<string>>(new Set());
  const [cooldown, setCooldown] = useState<ThermalCooldown | null>(null);
  const cooldownTimeoutRef = useRef<number | null>(null);

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
    invoke<SttSettings>("get_stt_settings")
      .then(setSttSettings)
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    invoke<LiveSubtitlesSettings>("get_live_subtitle_settings")
      .then((s) => setLiveSubtitlesEnabled(s.enabled))
      .catch((e) => setError(String(e)));
  }, []);

  // Parakeet has no "Auto-detect"/"Other" entries (see PARAKEET_LANGUAGE_OPTIONS),
  // so switching engines away from Whisper while either is selected would
  // leave the transcript-language picker on a value Parakeet can't use.
  useEffect(() => {
    if (sttSettings.engine === "parakeet" && !PARAKEET_LANGUAGE_CODES.has(languageOverride)) {
      setLanguageOverride("en");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sttSettings.engine]);

  function refreshRecordings() {
    invoke<RecordingSummary[]>("list_recordings")
      .then(setRecordings)
      .catch((e) => setError(String(e)));
  }

  useEffect(() => {
    refreshRecordings();
  }, []);

  // Debounced live search (F11): re-queries 300ms after the user stops
  // typing rather than on every keystroke, and clears results (rather than
  // showing an empty-results state) once the box is emptied.
  useEffect(() => {
    const trimmed = searchQuery.trim();
    if (!trimmed) {
      setSearchResults(null);
      setSearching(false);
      return;
    }
    setSearching(true);
    const handle = setTimeout(() => {
      invoke<SearchResult[]>("search_recordings", { query: trimmed })
        .then(setSearchResults)
        .catch((e) => setError(String(e)))
        .finally(() => setSearching(false));
    }, 300);
    return () => clearTimeout(handle);
  }, [searchQuery]);

  useEffect(() => {
    if (pipeline?.status !== "summarized" || !recordingIdRef.current) return;
    invoke<Summary | null>("get_summary", { recordingId: recordingIdRef.current })
      .then(setSummary)
      .catch((e) => setError(String(e)));
  }, [pipeline?.status]);

  // Rehydrates translatedLanguages/translating from the DB whenever the
  // selected recording changes -- both are otherwise pure React state that
  // only the translation-progress/translation-updated events populate, so a
  // page reload (or this app's own restart) would otherwise hide the
  // export/"Save" button and any in-flight translate job silently.
  useEffect(() => {
    const recordingId = pipeline?.id;
    if (!recordingId) return;
    invoke<TranslationStateResult>("get_translation_state", { recordingId })
      .then((result) => {
        if (recordingIdRef.current !== recordingId) return;
        setTranslatedLanguages(new Set(result.languages));
        if (result.inProgress) setTranslating(true);
      })
      .catch((e) => setError(String(e)));
  }, [pipeline?.id]);

  useEffect(() => {
    const unlisten = listen<RecordingSummary>("recording-updated", (event) => {
      if (event.payload.id === recordingIdRef.current) {
        setPipeline(event.payload);
        setTranscriptionProgress(event.payload.status === "transcribing" ? 0 : null);
        if (event.payload.status !== "summarizing") setCooldown(null);
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
    // Fired by the overlay toolbar's own Close button, by Esc inside the
    // overlay, and by the Rust side when the global hotkey closes it, so
    // this window's toggle button stays in sync regardless of trigger.
    const unlistenClosed = listen(EVENT_CLOSED, () => {
      setAnnotationOverlayVisible(false);
    });
    // Fired by the Rust side when the global hotkey opens the overlay, so
    // the toggle button also stays in sync in that direction.
    const unlistenOpened = listen(EVENT_OPENED, () => {
      setAnnotationOverlayVisible(true);
    });
    return () => {
      unlistenClosed.then((f) => f());
      unlistenOpened.then((f) => f());
    };
  }, []);

  useEffect(() => {
    // The engine can also be changed from the separate Settings window,
    // which has its own copy of this state in a different webview/JS
    // runtime — resync from the Rust-emitted broadcast (see
    // commands::set_stt_settings) rather than only reading it once on mount.
    const unlisten = listen<SttSettings>(EVENT_STT_SETTINGS_CHANGED, (event) => {
      setSttSettings(event.payload);
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
        setCooldown(null);
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // Backend polls the GPU every 5s while a local-LLM job (translate/summarize)
  // is paused for heat and re-emits this event each tick (see
  // thermal::wait_for_safe_temp) — there's no separate "resumed" event, so
  // clearing the banner on a timeout (a couple of ticks' worth) covers the
  // case where the temp drops and the job just continues silently.
  useEffect(() => {
    const unlisten = listen<ThermalCooldown>("thermal-cooldown", (event) => {
      if (event.payload.recordingId !== recordingIdRef.current) return;
      setCooldown(event.payload);
      if (cooldownTimeoutRef.current !== null) window.clearTimeout(cooldownTimeoutRef.current);
      cooldownTimeoutRef.current = window.setTimeout(() => setCooldown(null), 12_000);
    });
    return () => {
      unlisten.then((f) => f());
      if (cooldownTimeoutRef.current !== null) window.clearTimeout(cooldownTimeoutRef.current);
    };
  }, []);

  useEffect(() => {
    const unlisten = listen<TranslationProgress>("translation-progress", (event) => {
      if (event.payload.recordingId === recordingIdRef.current) {
        setTranslationProgress(event.payload.progress);
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
      setTranslationPaused(false);
      setTranslationProgress(null);
      setCooldown(null);
      if (event.payload.status === "done") {
        setTranslatedLanguages((prev) => new Set(prev).add(event.payload.language));
        setTranslationInfo(
          event.payload.failedSegments > 0
            ? `Translated to ${event.payload.language} — ${event.payload.failedSegments} of ${event.payload.totalSegments} segments kept the original text (translation failed for those).`
            : `Translated to ${event.payload.language}.`
        );
      } else if (event.payload.status === "cancelled") {
        setTranslationInfo("Translation stopped.");
      } else {
        setTranslationInfo(`Translation failed: ${event.payload.message ?? "unknown error"}`);
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // No `recordingId` on either event, unlike the ones above: while a
  // recording is in progress there's no DB recording id yet (one is only
  // minted in `stop_recording`), and this app only ever has one recording
  // active at a time, so nothing needs correlating.
  useEffect(() => {
    const unlisten = listen<LiveSubtitleCaption>("live-subtitle-caption", (event) => {
      setLiveCaption(event.payload.text);
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  useEffect(() => {
    const unlisten = listen<LiveSubtitleCooldown>("live-subtitle-cooldown", () => {
      setLiveSubtitlesCooldown(true);
      window.setTimeout(() => setLiveSubtitlesCooldown(false), 12_000);
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

  async function handleSaveLiveSubtitlesSettings(enabled: boolean) {
    setBusy(true);
    setError(null);
    try {
      await invoke("set_live_subtitle_settings", { settings: { enabled } });
      setLiveSubtitlesEnabled(enabled);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
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
    setTranslationPaused(false);
    setTranslationProgress(0);
    setTranslationInfo(null);
    try {
      await invoke("translate_subtitles", { recordingId: pipeline.id, targetLanguage });
    } catch (e) {
      setTranslating(false);
      setTranslationProgress(null);
      setError(String(e));
    }
  }

  async function handlePauseTranslation() {
    if (!pipeline) return;
    await run(() => invoke("pause_translation", { recordingId: pipeline.id }));
    setTranslationPaused(true);
  }

  async function handleResumeTranslation() {
    if (!pipeline) return;
    await run(() => invoke("resume_translation", { recordingId: pipeline.id }));
    setTranslationPaused(false);
  }

  async function handleStopTranslation() {
    if (!pipeline) return;
    // Backend discards whatever's already translated in this run rather than
    // persisting a partial result (see pipeline::run_translation) -- the
    // `translation-updated` "cancelled" event resets `translating` etc. once
    // the in-flight segment call finishes, so no local state reset here.
    await run(() => invoke("cancel_translation", { recordingId: pipeline.id }));
  }

  async function handleStart() {
    setPipeline(null);
    setTranscriptionProgress(null);
    setExportedPaths(null);
    setSummary(null);
    setTranslatedLanguages(new Set());
    setTranslationInfo(null);
    setTranslating(false);
    setTranslationPaused(false);
    setTranslationProgress(null);
    setCooldown(null);
    setLiveCaption("");
    setLiveSubtitlesCooldown(false);
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
    setLiveCaption("");
    setLiveSubtitlesCooldown(false);
    if (result) {
      recordingIdRef.current = result.recordingId;
      setPipeline({
        id: result.recordingId,
        videoPath: result.videoPath,
        createdAt: "",
        status: "recorded",
        detectedLanguage: null,
        durationMs: null,
        title: null,
      });
      setTranscriptionProgress(null);
      refreshRecordings();
    }
  }

  async function handleToggleAnnotationOverlay() {
    const nextVisible = !annotationOverlayVisible;
    await run(() => invoke(nextVisible ? "show_annotation_overlay" : "hide_annotation_overlay"));
    setAnnotationOverlayVisible(nextVisible);
  }

  function selectRecording(recording: RecordingSummary) {
    recordingIdRef.current = recording.id;
    setPipeline(recording);
    setTranscriptionProgress(null);
    setExportedPaths(null);
    setSummary(null);
    setTranslatedLanguages(new Set());
    setTranslationInfo(null);
    setTranslating(false);
    setTranslationPaused(false);
    setTranslationProgress(null);
    setCooldown(null);
  }

  function startEditingTitle(recording: RecordingSummary) {
    setEditingTitleId(recording.id);
    setEditingTitle(recording.title ?? "");
  }

  function cancelEditingTitle() {
    setEditingTitleId(null);
    setEditingTitle("");
  }

  async function handleRenameRecording(recording: RecordingSummary) {
    const nextTitle = editingTitle.trim() || null;
    setRenaming(true);
    setError(null);
    try {
      await invoke("rename_recording", { recordingId: recording.id, title: nextTitle });
      setRecordings((prev) =>
        prev.map((r) => (r.id === recording.id ? { ...r, title: nextTitle } : r))
      );
      setPipeline((prev) => (prev && prev.id === recording.id ? { ...prev, title: nextTitle } : prev));
      setEditingTitleId(null);
      setEditingTitle("");
    } catch (e) {
      setError(String(e));
    } finally {
      setRenaming(false);
    }
  }

  async function handleDeleteRecording(recording: RecordingSummary) {
    const confirmed = window.confirm(
      `Delete "${displayName(recording)}"? This removes its recording file, transcript, and summary. This cannot be undone.`
    );
    if (!confirmed) return;
    setBusy(true);
    setError(null);
    try {
      await invoke("delete_recording", { recordingId: recording.id });
      if (pipeline?.id === recording.id) {
        recordingIdRef.current = null;
        setPipeline(null);
      }
      if (editingTitleId === recording.id) {
        setEditingTitleId(null);
        setEditingTitle("");
      }
      refreshRecordings();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handleOpenRecordingFolder(recording: RecordingSummary) {
    try {
      await revealItemInDir(recording.videoPath);
    } catch (e) {
      setError(String(e));
    }
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
      <div className="header-row">
        <div>
          <h1>Screen Recorder — Capture Prototype</h1>
          <p className="subtitle">
            Week 3 milestone: LLM provider settings, transcript summaries.
          </p>
        </div>
        <button onClick={() => invoke("open_settings_window")}>Settings…</button>
      </div>

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
        <label className="checkbox-row path-row">
          Search by name, transcripts &amp; summaries
          <input
            type="text"
            value={searchQuery}
            placeholder="e.g. budget, action items…"
            onChange={(e) => setSearchQuery(e.target.value)}
          />
        </label>
        {searching && <p className="output">Searching…</p>}
        {searchResults !== null && (
          <div className="search-results">
            {searchResults.length === 0 && !searching && (
              <p className="output">No matches for "{searchQuery.trim()}".</p>
            )}
            {searchResults.map((result) => (
              <div
                key={result.recording.id}
                className={
                  "search-result" +
                  (result.recording.id === pipeline?.id ? " selected" : "")
                }
                onClick={() => selectRecording(result.recording)}
              >
                <div className="search-result-header">
                  <strong>{displayName(result.recording)}</strong>
                  <span>{PIPELINE_LABEL[result.recording.status]}</span>
                </div>
                {result.matches.map((match, i) => (
                  <p key={i} className="search-snippet">
                    {match.source === "transcript" && match.startMs !== null && (
                      <code>{formatTimecode(match.startMs)}</code>
                    )}
                    {match.source === "summary" && <code>summary</code>}
                    {match.source === "title" && <code>name</code>}
                    {" "}
                    {renderSnippet(match.snippet)}
                  </p>
                ))}
              </div>
            ))}
          </div>
        )}
        {searchResults === null && recordings.length === 0 && (
          <p className="output">No recordings yet.</p>
        )}
        {searchResults === null && recordings.length > 0 && (
          <table className="recordings-table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Date</th>
                <th>Duration</th>
                <th>Status</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {recordings.map((recording) => (
                <tr
                  key={recording.id}
                  className={recording.id === pipeline?.id ? "selected" : undefined}
                  onClick={() => selectRecording(recording)}
                >
                  <td onClick={(e) => e.stopPropagation()}>
                    {editingTitleId === recording.id ? (
                      <span className="row-actions">
                        <input
                          type="text"
                          autoFocus
                          value={editingTitle}
                          placeholder={formatCreatedAt(recording.createdAt)}
                          onChange={(e) => setEditingTitle(e.currentTarget.value)}
                          onKeyDown={(e) => {
                            if (e.key === "Enter") handleRenameRecording(recording);
                            if (e.key === "Escape") cancelEditingTitle();
                          }}
                        />
                        <button disabled={renaming} onClick={() => handleRenameRecording(recording)}>
                          Save
                        </button>
                        <button disabled={renaming} onClick={cancelEditingTitle}>
                          Cancel
                        </button>
                      </span>
                    ) : (
                      <span className="row-actions">
                        {displayName(recording)}
                        <button onClick={() => startEditingTitle(recording)}>Rename</button>
                      </span>
                    )}
                  </td>
                  <td>{formatCreatedAt(recording.createdAt)}</td>
                  <td>{formatDuration(recording.durationMs)}</td>
                  <td>
                    {PIPELINE_LABEL[recording.status]}
                    {recording.detectedLanguage && ` (${recording.detectedLanguage})`}
                  </td>
                  <td className="row-actions" onClick={(e) => e.stopPropagation()}>
                    <button onClick={() => handleOpenRecordingFolder(recording)}>Open folder</button>
                    <button
                      disabled={
                        recording.status === "transcribing" ||
                        recording.status === "summarizing" ||
                        (recording.id === pipeline?.id && translating)
                      }
                      onClick={() => handleDeleteRecording(recording)}
                    >
                      Delete
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
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
          <input
            type="checkbox"
            checked={liveSubtitlesEnabled}
            disabled={status !== "stopped" || !captureSystemAudio}
            onChange={(e) => handleSaveLiveSubtitlesSettings(e.currentTarget.checked)}
          />
          Live subtitles (experimental preview during recording, Windows + system audio only)
        </label>
        {liveSubtitlesEnabled && !captureSystemAudio && (
          <p className="output">
            Live subtitles need "System audio" enabled above — they won't run this
            session until it is.
          </p>
        )}

        <label className="checkbox-row">
          Transcript language
          <select
            value={languageOverride}
            disabled={status !== "stopped"}
            onChange={(e) => setLanguageOverride(e.currentTarget.value)}
          >
            {(sttSettings.engine === "parakeet" ? PARAKEET_LANGUAGE_OPTIONS : LANGUAGE_OPTIONS).map(
              (option) => (
                <option key={option.code} value={option.code}>
                  {option.label}
                </option>
              )
            )}
          </select>
        </label>
        {sttSettings.engine === "parakeet" && (
          <p className="output">
            Parakeet doesn't auto-detect the spoken language — pick one so summaries default to
            the right language.
          </p>
        )}
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
        <button disabled={busy} onClick={handleToggleAnnotationOverlay}>
          {annotationOverlayVisible ? "Hide annotation toolbar" : "Show annotation toolbar"}
        </button>
      </section>

      {status === "recording" && liveSubtitlesEnabled && captureSystemAudio && (
        <section className="live-caption-area">
          {liveCaption && <p className="live-caption">{liveCaption}</p>}
          {liveSubtitlesCooldown && (
            <p className="warning">GPU cooling down — live captions paused.</p>
          )}
        </section>
      )}

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
          {cooldown && (
            <p className="warning">
              GPU at {cooldown.gpuTempC}°C — pausing local LLM inference until it cools down
              (resumes below 80°C).
            </p>
          )}
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
                  <div className="controls">
                    <button disabled={busy || translating} onClick={handleTranslate}>
                      {translating ? "Translating…" : "Translate"}
                    </button>
                    {translating && (
                      <>
                        <button
                          disabled={busy}
                          onClick={translationPaused ? handleResumeTranslation : handlePauseTranslation}
                        >
                          {translationPaused ? "Resume" : "Pause"}
                        </button>
                        <button disabled={busy} onClick={handleStopTranslation}>
                          Stop
                        </button>
                      </>
                    )}
                  </div>
                  {translationPaused && <p className="output">Paused.</p>}
                  {translating && translationProgress !== null && (
                    <div className="progress-bar">
                      <div
                        className="progress-bar-fill"
                        role="progressbar"
                        aria-valuenow={Math.round(translationProgress * 100)}
                        aria-valuemin={0}
                        aria-valuemax={100}
                        style={{ "--progress": `${Math.round(translationProgress * 100)}%` } as CSSProperties}
                      />
                      <span className="progress-bar-label">
                        {Math.round(translationProgress * 100)}%
                      </span>
                    </div>
                  )}
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
