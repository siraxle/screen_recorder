import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "../App.css";

interface LiveSubtitleCaption {
  text: string;
}

// Standalone window showing every live-subtitle caption emitted so far
// this recording session (F6), unlike the main window's single-line
// "latest caption only" preview. Backfills via `get_live_subtitle_history`
// on open so a window opened mid-session doesn't start blank, then follows
// along live via the same `live-subtitle-caption` event the main window
// listens to (broadcast to every window, so no extra plumbing needed
// there) and resets on `live-subtitle-history-cleared` (emitted when a new
// recording starts, see `commands::start_recording`).
function LiveSubtitlesWindow() {
  const [history, setHistory] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    let cancelled = false;
    let unlistenCaption: (() => void) | undefined;
    let unlistenCleared: (() => void) | undefined;

    (async () => {
      // Subscribe before the backfill fetch resolves, not after: the
      // backend always records a caption into its history before emitting
      // it (see live_subtitles.rs's caption_task), so any caption this
      // listener picks up while the fetch below is still in flight is
      // guaranteed to already be included in that fetch's snapshot -- the
      // last-line check in the handler drops that would-be duplicate.
      unlistenCaption = await listen<LiveSubtitleCaption>("live-subtitle-caption", (event) => {
        const text = event.payload.text;
        if (!text) return;
        setHistory((prev) => (prev[prev.length - 1] === text ? prev : [...prev, text]));
      });
      unlistenCleared = await listen("live-subtitle-history-cleared", () => {
        setHistory([]);
      });
      try {
        const initial = await invoke<string[]>("get_live_subtitle_history");
        if (!cancelled) setHistory(initial);
      } catch (e) {
        if (!cancelled) setError(String(e));
      }
    })();

    return () => {
      cancelled = true;
      unlistenCaption?.();
      unlistenCleared?.();
    };
  }, []);

  useEffect(() => {
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [history]);

  return (
    <main className="container live-subtitles-window">
      <h1>Live subtitles</h1>
      <p className="subtitle">History for the current recording session.</p>
      {error && <p className="error">Error: {error}</p>}
      <div className="caption-history" ref={listRef}>
        {history.length === 0 && (
          <p className="hint">
            Waiting for captions&hellip; start a recording with live subtitles enabled (system
            audio required).
          </p>
        )}
        {history.map((line, i) => (
          <p key={i} className="caption-history-line">
            {line}
          </p>
        ))}
      </div>
    </main>
  );
}

export default LiveSubtitlesWindow;
