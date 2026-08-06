# CLAUDE.md

Guidance for Claude Code (or any AI coding agent) working in this repository.

## Project

A desktop application that records screen + audio, transcribes speech locally with
Whisper, and generates summaries via a pluggable LLM backend (local or cloud-by-token).
Full spec: `screen-recorder-spec.md` (Russian). This file governs day-to-day engineering
conventions; the spec governs product requirements and acceptance criteria.

Core pipeline: **Capture → Transcribe → Summarize → Index**, implemented as an async job
queue so recording is never blocked by heavy processing.

## Tech stack

- **Desktop shell:** Tauri (Rust core + web frontend). Do not introduce Electron.
- **Capture:** FFmpeg as a subprocess, driven per-OS:
  - Windows: `ddagrab`/`gdigrab` (video) + WASAPI loopback (system audio) + WASAPI (mic)
  - macOS: `avfoundation`/ScreenCaptureKit (video) + ScreenCaptureKit audio tap (system audio, macOS 13+)
  - Linux: `x11grab` (X11 only for MVP; Wayland is best-effort, do not block MVP work on it) + PulseAudio/PipeWire monitor
- **STT:** Whisper via runtime selected by target hardware — never hardcode one runtime:
  - `faster-whisper` (CTranslate2) on NVIDIA/CUDA
  - `whisper.cpp` on Apple Silicon (Metal/Core ML) and CPU-only machines
  - `WhisperX` when word-level timestamps and speaker diarization are needed (wraps faster-whisper + wav2vec2 alignment + pyannote-audio)
  - Always run VAD before Whisper inference — long silences cause hallucinated output otherwise.
  - Default model: `small` or `distil-large`. Do not default to `large` "just in case" — surface a size/speed/VRAM tradeoff picker to the user instead.
  - **Multilingual is a hard requirement — never Russian-only, never English-only.** Never load `.en`-suffixed model variants (e.g. `small.en`) as a default; they cannot transcribe non-English audio at all. Always use the multilingual model checkpoint. Auto-detect language from the first ~30s of audio (native Whisper behavior), with a manual override in recording settings. When picking a "fast" distilled checkpoint, verify it's a multilingual build before adding it to the model picker — some distil-whisper checkpoints are English-only.
  - Summary generation must also be language-aware: pass the transcript's language to the LLM prompt and default to responding in that language, but let the user set a separate summary-language override (e.g. English transcript → Russian summary is a valid, supported case).
- **LLM Provider Abstraction:** same pattern as the [[anki-generator]] project —
  - Local provider: any OpenAI-compatible endpoint (Ollama, LM Studio, llama.cpp server, vLLM)
  - Cloud provider: user-supplied API token (Anthropic/OpenAI/compatible)
  - Never hardcode a single provider into business logic — route through the abstraction layer.
  - Store cloud tokens in OS keychain / encrypted local store. Never in plain-text config, never logged.
- **Storage:** SQLite with FTS5 for transcripts/summaries full-text search. Media files (mp4/audio) on disk, referenced by path in SQLite — do not blob video into the DB.

## Repository conventions

- Keep the Rust core (capture orchestration, job queue, FFmpeg process management) separate
  from the STT/LLM worker processes — they should be swappable subprocesses/services, not
  tightly coupled libraries, so a runtime can be replaced without touching the orchestrator.
- Job queue stages (`recorded → transcribing → transcribed → translating(optional) → summarizing → summarized → indexed`)
  must be explicit and persisted — if the app crashes mid-pipeline, it must resume from the
  last completed stage on restart, not from scratch. `translating` only runs when the user's
  subtitle language differs from the video's spoken language; it must not block `summarizing`
  from starting — they can run concurrently.
- Subtitle language and video (spoken) language are two distinct settings — never conflate them.
  Default subtitle language = detected/selected video language (no translation needed). When the
  user picks a different subtitle language, translate the transcript segment-by-segment (not the
  concatenated text) so timecodes stay aligned with the source segments.
- Dual/bilingual subtitles: default export is a single .srt/.vtt with two lines per cue (original +
  translation) for maximum player compatibility. Offer a secondary export mode producing two
  separate files (`original.<lang>.srt` + `translated.<lang>.srt`) with matching timecodes, for
  players that support a secondary subtitle track (VLC, mpv). Do not build a single-file "two
  independently toggleable tracks" format — no common container guarantees that; keep the two
  approaches as two distinct, clearly-labeled export options instead.
- Any feature that sends data off-device (cloud LLM calls) must have a single, auditable
  code path. Do not scatter cloud API calls across multiple modules.
- Privacy zones (masked screen regions) must be applied to the video *before* it touches
  disk or is fed to STT/LLM — never redact after the fact.

## What "done" means for a feature

Every feature ships against the Given/When/Then acceptance criteria in
`screen-recorder-spec.md`. When implementing a feature:
1. Find its criteria in the spec (section 6, tagged F1, F2, …).
2. Write/adjust tests against those criteria before considering the feature complete.
3. Do not silently expand scope beyond what's specified for the current milestone
   (MVP / v1.1 / v2) — flag scope questions instead of guessing.

## Known constraints (do not "fix" without discussion)

- Speaker diarization is inherently unreliable industry-wide (~60-70% accuracy under
  noise/accents even in commercial products). Label it experimental in the UI; do not
  present it as production-grade accuracy.
- Wayland screen+audio capture has no stable cross-compositor API as of 2026 — treat
  as best-effort, not a blocker for other work.
- macOS system audio capture (ScreenCaptureKit/Core Audio Taps) has real API limitations
  around per-app audio isolation and privacy permission prompts — budget time for
  permission-flow edge cases, don't assume a clean one-shot capture.

## Commands

- Dev (desktop app, hot reload): `npm run tauri dev`
- Build release bundle: `npm run tauri build`
- Frontend type-check + build only: `npm run build`
- Rust checks: `cd src-tauri && cargo check`
- Rust tests (includes a real end-to-end capture test — spawns ffmpeg and a
  real WASAPI loopback capture, no mocks): `cd src-tauri && cargo test`

Prerequisites on Windows: Rust (via rustup, MSVC toolchain) and the MSVC C++
Build Tools (`Microsoft.VisualStudio.Workload.VCTools`) — both are installed
on this dev machine already. FFmpeg must be on PATH.
