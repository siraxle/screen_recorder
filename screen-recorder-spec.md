# Specification: Local Screen Recording App with Subtitles and Summaries

**Status:** draft v0.1
**Date:** July 28, 2026
**Requirements author:** Evgeniy

---

## 1. Summary

A desktop application that:
1. Records screen video and audio (system audio + microphone).
2. Transcribes speech via Whisper (locally) → generates subtitles (SRT/VTT) with timecodes.
3. Builds a summary of the recording (brief overview, key points, action items) via an LLM — either local (Ollama/LM Studio/llama.cpp) or cloud, via API token (Anthropic/OpenAI/other).
4. Works fully offline in its base mode (privacy-first), with an optional switch to the cloud for higher summary quality.

---

## 2. Market analysis (July 2026)

The market splits into three clear categories. Understanding their trade-offs determines which niche to target.

### 2.1. SaaS "meeting assistants" (bot-based)
**Otter.ai, Fireflies.ai, Fathom, Read AI, Avoma, Grain**

- Work via a bot that joins the call (Zoom/Meet/Teams), or record only during online meetings.
- <cite index="9-1">Some tools like Convo, Granola, and a few others capture audio locally without joining the meeting as a participant</cite> — this is the "bot-free" subcategory.
- <cite index="3-1">Fireflies.ai supports 60+ languages — the most in the category; Fathom covers 38 languages with reduced accuracy on accents; Granola supports 10 languages on desktop</cite>.
- <cite index="5-1">Independent 2026 reviews rate speaker recognition as the category's weak point: accuracy drops to 60–70% under noise or accents, with speaker-attribution errors reaching ~30% in multi-participant conversations</cite>.
- Key takeaway for our product: **these tools are tied to online calls** (Zoom/Meet/Teams) and barely address the "record an arbitrary screen + whatever audio is playing on the system" use case (tutorials, streams, training videos, working with any software, not just video calls).
- All are monetized through the cloud — there is no local mode; data goes to the vendor's servers.

### 2.2. Open-source "screen memory" / privacy-first
**Screenpipe, OpenRecall**

- <cite index="12-1">Screenpipe is an open-source (MIT) app that continuously records screen and audio, building a local, searchable AI memory of everything happening on the computer</cite>.
- <cite index="11-1">Audio: Whisper (local, Large-V3-Turbo) or Deepgram (cloud) for speech-to-text, with speaker identification and diarization; storage — local SQLite with FTS5 full-text search</cite>.
- <cite index="11-1">Instead of continuously recording every second, Screenpipe listens for meaningful events (app switches, clicks, typing pauses, scrolling) and only takes a screenshot when something actually changed</cite> — saving disk and CPU (~300 MB over 8 hours vs. ~2 GB with continuous recording).
- <cite index="11-1">The UI is built on Tauri (Rust + TypeScript)</cite>, with a localhost REST API and plugin-agents in the form of markdown files.
- This is the closest conceptual competitor to our idea, but Screenpipe's focus is "computer memory" for search and AI agents, not a ready-made "subtitles + structured summary of a single recording" pipeline for the end user.

### 2.3. Classic recorders + separate transcription tools
**OBS Studio, Loom, Screen Studio + Otter/Descript/Trint as a separate step**

- OBS/Screen Studio only record, with no built-in STT/summary — the user manually runs the file through a third-party service.
- <cite index="8-1">2026 reviews compare transcription tools separately from recorders: Otter, VexaScribe, Rev, Descript, Trint, Sonix, Temi, Granola, Fathom, Fireflies, AssemblyAI Universal-2, Deepgram Nova-3, and the open-source combo Whisper Large-v3-turbo / whisper.cpp / faster-whisper / WhisperX</cite>.
- The gap between "recorded" and "got a structured writeup" is usually closed manually by the user, stitching together 2-3 tools.

### 2.4. Niche and positioning for Evgeniy's product
An open niche at the intersection of:
- recording an **arbitrary** screen (not just video calls) — tutorials, lectures, coding sessions, streams, training sessions;
- a **fully local** STT + LLM pipeline by default (unlike SaaS meeting assistants);
- flexible switching to a cloud LLM via token for higher-quality summaries when privacy isn't critical (unlike Screenpipe, which emphasizes "memory" rather than a "ready-made meeting/lecture writeup").

**Key competitor gaps worth closing:**
- Speaker diarization is weak across the board — worth explicitly labeling "experimental" and not overpromising.
- SaaS tools don't offer an offline mode — our #1 differentiator.
- Screenpipe has no one-click summary of a single specific recording (its tracks are built for general history search, not a specific session).

---

## 3. Technology stack and approaches

### 3.1. Screen and audio capture

| OS | Video | System audio | Microphone |
|---|---|---|---|
| Windows | `ddagrab`/`gdigrab` (FFmpeg) | WASAPI loopback | WASAPI |
| macOS | `avfoundation` / ScreenCaptureKit | ScreenCaptureKit (audio tap, macOS 13+) | AVFoundation |
| Linux | `x11grab` (X11); Wayland — limited support | PulseAudio/PipeWire monitor loopback | PulseAudio/PipeWire |

Rationale:
- <cite index="36-1">FFmpeg covers screen capture on Windows, macOS, and Linux via gdigrab/ddagrab/avfoundation/x11grab, including simultaneous system-audio recording</cite>.
- <cite index="35-1">On Windows, system audio is captured via WASAPI in loopback mode</cite>; on macOS, Apple <cite index="35-1">relatively recently opened up system audio as a separate audio source for apps, but even native APIs like ScreenCaptureKit and Core Audio Taps have a number of limitations</cite> — budget time for edge cases (privacy permissions, missing audio from certain apps).
- Linux/Wayland — consciously accept as "best effort" for MVP, support fully later (no stable cross-compositor API for screen+audio capture).

**Recommendation:** don't reinvent the wheel on top of bare FFmpeg CLI — use it as a worker process (spawn + pipe), or a ready-made wrapper at the level of `audio-capture` (WASAPI/ScreenCaptureKit) for system audio if low latency without virtual-cable hacks (VB-Cable/BlackHole) is needed.

### 3.2. Speech recognition — Whisper

Not "just Whisper," but a runtime choice based on hardware:

| Runtime | When to use |
|---|---|
| **faster-whisper** (CTranslate2) | NVIDIA GPU — <cite index="19-1">int8 quantization gives the best speed/VRAM ratio on CUDA</cite> |
| **whisper.cpp** | Mac (Metal/Core ML) or CPU-only — <cite index="19-1">a C/C++ port with no Python dependencies, uses Metal and Core ML on Apple Silicon</cite> |
| **WhisperX** | Need subtitles with precise word-level timecodes and diarization — <cite index="22-1">uses faster-whisper for base ASR, then forced alignment via wav2vec2 for word-level timecodes, and pyannote-audio for speaker diarization</cite> |

Practical recommendations:
- **Multilingual support is a hard requirement**, not Russian-only. Whisper is inherently multilingual (trained on 680,000 hours of multilingual audio), but many presets and tutorials default to English-only model variants (the `.en` suffix — e.g., `small.en`) — these **must not** be used by default, since they are physically incapable of recognizing anything but English. The app must always load the multilingual variant of the model (`small`, `medium`, `large-v3-turbo`, etc., without `.en`).
- Auto-detect language at the start of transcription (Whisper does this natively from the first ~30 seconds of audio) + the ability to manually override the language in settings if auto-detection gets it wrong (short clips, mixed speech, specific terminology).
- Support the scenario of a single recording containing multiple languages (code-switching) at least at the "doesn't break" level, even if recognition quality for mixed speech is lower — Whisper handles this in a limited way; no dedicated solution is planned for MVP.
- distil-whisper (upstream distilled models) was historically English-only in its first versions — when picking a "fast" model, explicitly verify that the specific checkpoint is multilingual, otherwise don't include it in the default option list.
- <cite index="22-1">VAD (voice activity detection) must always be applied — without it, Whisper tends to hallucinate during long silences</cite>; this is independent of language.
- Unified architecture: **faster-whisper on CUDA / whisper.cpp on Mac and CPU-only**, with WhisperX as an optional "precise subtitles + diarization" mode (heavier, requires a HuggingFace token for pyannote). WhisperX supports the same set of languages as base Whisper, but the forced-alignment model (wav2vec2) for word-level timecodes doesn't exist for every language — when no alignment model is available for the selected language, WhisperX should fall back to regular (non-word-level) timecodes rather than fail with an error.
- For diarization, explicitly warn the user about experimental quality (see section 2.2).

**Summary generation must also be multilingual on the LLM side:** summarization prompt templates must either explicitly pass the transcript's language to the LLM and ask it to respond in that same language, or give the user a separate "summary language" setting (e.g., an English transcript but a summary needed in Russian — a valid use case for material that isn't in the user's native language).

### 3.3. LLM for summaries — hybrid abstraction

Same pattern already used in the [[anki-generator]] project (LLM Provider Abstraction):

- **Local provider:** an OpenAI-compatible endpoint — <cite index="33-1">Ollama exposes its API immediately after the machine starts, with no manual launch needed, removing friction for apps calling a local model</cite>; LM Studio — as an option for users who prefer a GUI and manual model selection.
- **Cloud provider:** any OpenAI/Anthropic-compatible API via a user-supplied token (store in OS keychain / encrypted local store, never in a plain-text config).
- Recommended local models for the "summarize/structure text" task (not code generation): <cite index="28-1">Qwen 3 8B — good multilingual support and reasoning; Gemma 3 4B — handles summarization, Q&A, and light tasks confidently with much lower hardware requirements</cite>.
- Routing strategy (optional, v2): <cite index="33-1">the local model handles most routine tasks (summarization, formatting, drafts), while the cloud model handles the remainder where top-tier quality is needed</cite> — i.e., automatic fallback to the cloud on low confidence/complex text, if the user has a token configured.

### 3.4. Desktop framework

- **Tauri** (Rust backend + web frontend) — the same choice as Screenpipe: <cite index="11-1">a desktop app built on Tauri (Rust + TypeScript)</cite>. Smaller binary size and lower idle memory usage than Electron — important for a background recording process.
- Alternative — Electron, if development speed and reuse of the JS ecosystem are the priority; the trade-off is a larger idle footprint.

### 3.5. Data storage

- Recording metadata, transcripts, summaries — local SQLite (+ FTS5 for full-text search over subtitles/summaries, similar to Screenpipe's approach).
- Video/audio — files on disk (mp4/webm + a separate audio track if needed).
- Storage encryption — an option for sensitive recordings (user-supplied key), off by default for performance, with an explicit UI warning.

### 3.6. Subtitle translation and dual subtitles

The user needs to distinguish two independent settings:
- **Video language** (the spoken language in the recording) — auto-detected by Whisper or set manually (see 3.2).
- **Subtitle language** — defaults to the video language (a direct transcript, no translation). If the user sets a different subtitle language, an additional **translation** step via LLM is required, preserving segment timecodes (each segment's text is translated individually, not the concatenated full text, otherwise timecodes would drift).

**Dual (bilingual) subtitles** — showing the original and the translation simultaneously. Technically there are two independent approaches, both worth supporting:
- **Option A — single file, two lines per cue.** One .srt/.vtt file where each cue's first line is the original and the second is the translation. Maximally compatible (works in any player, on YouTube, etc.), but the user can't toggle the translation on/off independently of the original — it's a "baked-in" pair.
- **Option B — two separate files as primary and secondary track.** Exports `original.srt` + `translated.srt` separately; players that support a secondary subtitle track (VLC, mpv) let both tracks be shown at once and controlled independently (e.g., resizing only the translation).

Recommendation: **Option A by default** (two-line cue) as the most predictable choice for end users without a specific player; Option B as an additional export option for advanced users.

Translation is a separate job-queue task (`translating`), run after `transcribed`, but does not block `summarizing` — the summary can be generated in parallel with subtitle translation.

---

## 4. User-facing features

### Must-have (MVP)
- Screen recording (full screen / specific window / region) + system audio + microphone, with the ability to disable any source.
- Pause/resume recording.
- Local transcription via Whisper after recording (batch mode), with automatic speech-language detection and manual language override — not Russian-only, support for all languages covered by multilingual Whisper models.
- Export subtitles as SRT and VTT. Subtitle language defaults to the video language (auto-detected or manually set); the "subtitle language" setting is kept separate from the "video language" setting.
- Text summary of the recording (brief overview + list of key points) via a local or cloud LLM, with the ability to choose the summary language independently of the transcript language.
- LLM provider (local/cloud) and API token configuration in the UI.
- Recordings list with basic search by title/date.

### Should-have (v1.1)
- Live subtitles over the recording in real time (streaming STT).
- Speaker diarization (WhisperX), explicitly marked "experimental."
- Translated subtitles: choose a subtitle language different from the video language, translating the transcript via LLM segment-by-segment (preserving timecodes).
- Dual (bilingual) subtitles: simultaneous display of original and translation — by default as a two-line cue in a single file, optionally exporting two separate files (primary + secondary track) for players that support a second track.
- Structured summary: breakdown into chapters/timecodes, action items, key decisions (a format applicable to both lectures and work meetings).
- Full-text search across transcripts and summaries of all recordings (FTS5).
- Choice of Whisper model size and local LLM, with an indication of expected speed/quality for the current hardware.
- Hotkeys for start/stop/pause.
- Private screen zones (masking a region, e.g., a password manager) — cut out of the video and not passed to STT/LLM.
- Webcam picture-in-picture overlay: composite the front camera into a chosen corner of the recording, as a small circle or square.
- Camera-only recording: record the front camera full-frame instead of the screen, in the camera's native mode, mirrored the way the user sees themselves, with the microphone as the only audio source.

### Nice-to-have (v2)
- Automatic local/cloud LLM routing based on task complexity.
- Summary plugins/templates (lecture, code review, daily standup, interview) with different prompt templates.
- Export summaries to Markdown/Notion/Obsidian.
- Local storage encryption "on by default" with hardware acceleration.
- Subtitle translation into another language via LLM (separate from the base multilingual speech recognition, which is already mandatory in MVP — see F2/F3).
- Localization of the app's own UI into multiple languages.
- Clips: automatically cutting the recording into short fragments based on summary chapters.

---

## 5. Architecture (components)

```
┌─────────────────────────────────────────────────────────┐
│                     Desktop App (Tauri)                  │
│  ┌───────────────┐  ┌───────────────┐  ┌──────────────┐  │
│  │  Capture       │  │  Recordings    │  │  Settings    │  │
│  │  Controller    │  │  Library UI    │  │  UI          │  │
│  └───────┬───────┘  └───────┬───────┘  └──────┬───────┘  │
│          │                  │                  │          │
│  ┌───────▼──────────────────▼──────────────────▼───────┐  │
│  │              Rust Core / Orchestrator                │  │
│  │  - FFmpeg process manager (video + audio capture)    │  │
│  │  - Job queue: transcribe → summarize → index          │  │
│  └───────┬──────────────────┬──────────────────┬───────┘  │
│          │                  │                  │          │
│  ┌───────▼──────┐  ┌────────▼────────┐  ┌──────▼───────┐  │
│  │ STT Worker    │  │ LLM Provider     │  │ SQLite +     │  │
│  │ (faster-      │  │ Abstraction      │  │ FTS5 Store   │  │
│  │  whisper /    │  │ - Local (Ollama/ │  │              │  │
│  │  whisper.cpp/ │  │   LM Studio)     │  │              │  │
│  │  WhisperX)    │  │ - Cloud (token)  │  │              │  │
│  └───────────────┘  └──────────────────┘  └──────────────┘  │
└─────────────────────────────────────────────────────────┘
```

Core principle: **Capture → Transcribe → Summarize → Index** — an asynchronous pipeline with a job queue, so recording is never blocked by heavy processing.

---

## 6. Specification with acceptance criteria (Given/When/Then)

### Milestone 1 — MVP (weeks 1–4)

**F1. Screen and audio recording**
- Given the user has opened the app and selected a screen source (full screen/window/region)
  When they click "Start Recording"
  Then the app begins capturing video and the selected audio sources (system audio and/or microphone) and shows an active-recording indicator.

- Given a recording is in progress
  When the user clicks "Pause"
  Then video/audio capture pauses without losing the already-recorded segment, and the button changes to "Resume."

- Given a recording is in progress
  When the user clicks "Stop"
  Then the recording file is saved to disk in a readable format (mp4), and the recording appears in the library with status "Awaiting processing."

- Given more than one monitor is connected and the user selected one specific monitor in the monitor picker
  When they record the screen (with or without the webcam overlay)
  Then the saved video contains only that monitor, at its native resolution, and the webcam overlay (if enabled) is placed in the chosen corner of that monitor.

- Given more than one monitor is connected and the user selected "All monitors" (the default)
  When they record the screen
  Then the saved video contains every monitor side by side, exactly as before monitor selection existed.

- Given the user selected a specific monitor and that monitor is disconnected before recording starts
  When they click "Start Recording"
  Then the app shows a clear error and does not start recording (it never silently falls back to recording all monitors).

**F2. Local transcription**
- Given a recording is saved and a Whisper worker is available locally
  When the automatic pipeline starts transcription
  Then a text transcript with timecodes is generated for the recording, and the status changes to "Transcribed."

- Given a transcript is ready
  When the user clicks "Export subtitles"
  Then the app generates .srt and .vtt files with timecodes matching the video.

- Given the recording contains speech in a language other than Russian (e.g., English, Spanish, German, etc.)
  When transcription runs with the "auto-detect language" setting
  Then Whisper detects the language from the audio and generates the transcript in that language, without forcing use of the English-only (`.en`) model.

- Given auto-detection got the language wrong, or the user already knows the recording's language
  When the user manually specifies the language in settings before transcription
  Then the pipeline uses the specified language instead of auto-detection.

- Given the user has not changed the "subtitle language" setting
  When subtitles are generated
  Then the subtitle language automatically matches the detected (or manually set) video language, with no extra action from the user.

**F3. Summary**
- Given a transcript is ready and an LLM provider is configured in settings (local, or cloud with a valid token)
  When the user triggers summary generation
  Then the app returns a text summary (brief overview + list of key points) and saves it alongside the recording.

- Given a cloud provider is selected but the token is invalid or missing
  When the user triggers summary generation
  Then the app shows a clear error and does not send any data to the external service.

- Given a transcript in a language other than Russian, and the user has not specified a separate summary language
  When summary generation runs
  Then the prompt template explicitly passes the transcript's language to the LLM and asks for the summary in that same language.

- Given the user has explicitly specified a summary language different from the transcript's language
  When summary generation runs
  Then the result is returned in the language the user specified, not the transcript's original language.

**F4. Provider settings**
- Given the user has opened LLM settings
  When they select "Local" and provide an Ollama/LM Studio endpoint address
  Then the app checks the endpoint's availability and shows the connection status.

- Given the user selects "Cloud"
  When they enter an API token
  Then the token is saved in encrypted local storage (not in a plain-text config) and is never logged.

**F5. Recordings library**
- Given one or more saved recordings exist
  When the user opens the main screen
  Then they see a list of recordings with date, duration, and processing status (recorded/transcribed/summarized).

### Milestone 2 — v1.1 (weeks 5–8)

**F6. Live subtitles**
- Given a recording is in progress and the "Live subtitles" option is enabled
  When the user speaks or system audio plays
  Then subtitles are displayed over the recording UI with no more than ~2–3 seconds of delay.

**F7. Speaker diarization (experimental)**
- Given a recording contains multiple voices and the WhisperX diarization option is enabled
  When the transcript is generated
  Then each utterance is tagged with a provisional speaker label (Speaker 1/2/…), and the UI shows a warning: "Diarization is experimental; attribution errors are possible."

**F8. Translated subtitles**
- Given a transcript is ready and the user selected a subtitle language different from the video language
  When subtitle generation runs
  Then the transcript is translated into the selected language via LLM, segment-by-segment (preserving each segment's timecodes), and the result is available for export as .srt/.vtt.

- Given LLM translation of a segment failed (timeout, provider unavailable)
  When subtitle generation continues for the remaining segments
  Then the failed segment is marked as untranslated (the original is kept as a fallback) rather than aborting the entire process.

**F9. Dual (bilingual) subtitles**
- Given the user enabled "Dual subtitles" and specified the video language and translation language
  When subtitles are generated
  Then, by default, a single .srt/.vtt file is exported where each cue contains two lines: original and translation.

- Given the user selected "Export as separate files" with dual subtitles enabled
  When generation completes
  Then the app saves `original.<lang>.srt` and `translated.<lang>.srt` as separate files with identical timecodes.

**F10. Structured summary**
- Given a transcript is ready
  When the user triggers "Structured summary"
  Then the result includes: chapters with timecodes, a list of decisions/conclusions, and a list of action items (where applicable to the recording type).

**F11. Search across recordings**
- Given several recordings exist with completed transcripts/summaries
  When the user enters a search query
  Then the app returns a list of recordings where the query appears, with match highlighting and a timecode.

**F12. Private zones**
- Given the user marked a screen region as a "private zone" before starting the recording
  When recording is in progress
  Then that region is hidden (blurred/blacked out) in the saved video and is not passed to STT/LLM.

**F16. Webcam overlay**
- Given the user enabled "Webcam overlay" and selected a camera, shape (circle/square), and corner before starting the recording
  When they click "Start Recording"
  Then the saved video shows the camera feed composited into the chosen corner, in the chosen shape, for the whole recording.

- Given "Webcam overlay" is not enabled
  When a recording is made
  Then the saved video is unaffected — screen (and audio) capture only, exactly as before this feature existed.

- Given "Webcam overlay" is enabled but no camera is available on the machine
  When the user clicks "Start Recording"
  Then the app shows a clear error and does not start recording, matching how a missing microphone is handled.

**F17. Camera-only recording**
- Given the user enabled "Camera only (no screen)" and selected a camera before starting the recording
  When they click "Start Recording"
  Then the saved video is the camera feed full-frame, in the camera's native resolution and frame rate, mirrored horizontally (as the user sees themselves), with no screen capture in it.

- Given "Camera only" is enabled and "Microphone" is enabled
  When a recording is made
  Then the saved video contains the microphone audio as its only audio track; system audio is not captured in this mode, even if it was selected before switching to camera-only.

- Given "Camera only" is enabled and "Microphone" is disabled
  When a recording is made
  Then the saved video has a video stream and no audio stream.

- Given "Camera only" is enabled
  When the user pauses and resumes the recording
  Then the saved video contains both fragments, exactly as for screen recording (F1).

- Given "Camera only" is not enabled
  When a recording is made
  Then recording behaves exactly as before this feature existed (screen, audio-only, and webcam overlay modes are unaffected).

- Given "Camera only" is enabled but no camera is available on the machine
  When the user clicks "Start Recording"
  Then the app shows a clear error and does not start recording, matching F16.

### Milestone 3 — v2 (weeks 10–15)

**F13. LLM auto-routing**
- Given both providers (local and cloud) are configured
  When a summary is being generated and the local model returns low confidence or exceeds the context limit
  Then the task is automatically redirected to the cloud provider, and the user is notified.

**F14. Summary templates**
- Given the user selected a recording type (lecture/code review/standup/interview) before generating a summary
  When summarization runs
  Then the corresponding prompt template is used, producing a structure appropriate to the recording type.

**F15. Export to external formats**
- Given a summary is ready
  When the user selects "Export to Markdown"
  Then an .md file is generated with the summary, chapter timecodes, and links to video fragments.

---

## 7. Weekly roadmap

| Week | Focus |
|---|---|
| 1 | Prototype screen+audio capture via FFmpeg on the developer's target OS; save mp4 |
| 2 | Integrate faster-whisper/whisper.cpp; batch transcription of a completed recording, SRT/VTT export |
| 3 | LLM Provider Abstraction (local Ollama + one cloud provider via token); summary generation |
| 4 | Recordings library UI, provider settings, pipeline statuses; **MVP done** |
| 5 | Live subtitles (streaming STT) |
| 6 | WhisperX diarization as an optional mode |
| 7 | Subtitle translation (LLM, segment-by-segment) + dual subtitles (single file and separate export) |
| 8 | Structured summary (chapters, action items) + SQLite FTS5 search |
| 9 | Private screen zones; **v1.1 done** |
| 10–11 | Local/cloud LLM auto-routing |
| 12–13 | Summary templates by recording type |
| 14–15 | Export to Markdown/external formats, polish; **v2 done** |

---

## 8. Risks and constraints

- **Wayland (Linux):** no stable cross-compositor API for screen+audio capture — treat Linux as best-effort for MVP, with full support deferred to a later stage.
- **Diarization:** quality is low even among market leaders (see section 2.1) — don't promise the user accuracy; explicitly mark it as an experimental feature.
- **macOS system audio:** native API limitations (ScreenCaptureKit/Core Audio Taps) require budgeting time for edge cases and privacy permissions.
- **Performance:** batch transcription of long recordings on weaker hardware can take time comparable to the recording's duration — an explicit progress bar is needed, along with the ability to keep using the app while processing runs in the background.
- **Data privacy:** when using a cloud provider, clearly warn the user which data (the transcript, not the video) leaves the device.
- **Uneven recognition quality across languages:** Whisper's accuracy varies significantly by language (best for languages with large training-data volumes, worse for low-resource languages) — UX should not assume uniform quality expectations regardless of the recording's language.
