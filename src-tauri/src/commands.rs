use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::capture::{ffmpeg, CaptureConfig, CaptureController, RecordingStatus};
use crate::db::{self, RecordingSummary, Segment, Summary};
use crate::hotkeys::{self, HotkeySettings};
use crate::llm::{self, LlmSettings, ProviderKind};
use crate::overlay;
use crate::pipeline::live_subtitles::{self, LiveSubtitles, LiveSubtitlesSettings};
use crate::pipeline::stt::{self, SttSettings};
use crate::settings_window;
use crate::state::AppState;
use crate::subtitle;

#[tauri::command]
pub async fn list_microphones() -> Result<Vec<String>, String> {
    ffmpeg::list_microphones().await.map_err(|e| e.to_string())
}

/// `start_recording`'s input: capture source selection plus the
/// transcription-language setting for this recording (F2's "user manually
/// specifies the language in settings before transcription" criterion).
/// `language_override: None` means auto-detect.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingSettings {
    pub capture: CaptureConfig,
    pub language_override: Option<String>,
}

#[tauri::command]
pub async fn start_recording(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    settings: RecordingSettings,
) -> Result<(), String> {
    let mut guard = state.controller.lock().await;
    if guard.is_some() {
        return Err("a recording is already in progress".to_string());
    }

    // F6 live subtitles: only wired up when the setting is on *and* system
    // audio capture is enabled this session (system audio is the only tap
    // implemented so far, see live_subtitles.rs's module doc) -- never
    // blocks the recording itself if either isn't the case.
    let live_settings = db::with_db(&state.db, |conn| live_subtitles::load_settings(conn))
        .await
        .map_err(|e| e.to_string())?;
    let live_tap = if live_settings.enabled && settings.capture.capture_system_audio {
        match LiveSubtitles::start(app.clone(), settings.language_override.clone()).await {
            Ok((live, tap_tx)) => {
                *state.live_subtitles.lock().await = Some(live);
                Some(tap_tx)
            }
            Err(e) => {
                tracing::warn!("failed to start live subtitles, continuing without them: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let recordings_dir = state.recordings_dir.lock().await.clone();
    let mut controller = CaptureController::new(
        settings.capture,
        &recordings_dir,
        settings.language_override,
        live_tap,
    )
    .map_err(|e| e.to_string())?;
    controller.start().await.map_err(|e| e.to_string())?;
    *guard = Some(controller);
    Ok(())
}

#[tauri::command]
pub async fn pause_recording(state: State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.controller.lock().await;
    let controller = guard.as_mut().ok_or("no recording in progress")?;
    controller.pause().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn resume_recording(state: State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.controller.lock().await;
    let controller = guard.as_mut().ok_or("no recording in progress")?;
    controller.resume().await.map_err(|e| e.to_string())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopRecordingResult {
    pub recording_id: String,
    pub video_path: String,
}

#[tauri::command]
pub async fn stop_recording(
    state: State<'_, AppState>,
) -> Result<StopRecordingResult, String> {
    let mut guard = state.controller.lock().await;
    let mut controller = guard.take().ok_or("no recording in progress")?;
    drop(guard);

    // Each recording gets its own folder (video + later subtitle exports),
    // rather than dumping everything flat into recordings_dir.
    let recordings_dir = state.recordings_dir.lock().await.clone();
    let folder_name = format!(
        "recording-{}",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
    );
    let recording_folder = recordings_dir.join(folder_name);
    std::fs::create_dir_all(&recording_folder).map_err(|e| e.to_string())?;
    let output_path = recording_folder.join("recording.mp4");
    let language_override = controller.language_override().map(str::to_string);
    let result_path = controller
        .stop(&output_path)
        .await
        .map_err(|e| e.to_string())?;
    // Dropping the controller here (rather than at function end) drops its
    // clone of the live-subtitles tap sender, which is what lets that
    // pipeline's EOF-driven shutdown in `LiveSubtitles::stop` begin
    // immediately instead of only once this whole command returns.
    drop(controller);
    if let Some(live) = state.live_subtitles.lock().await.take() {
        live.stop().await;
    }
    // Best-effort: a missing/broken ffprobe shouldn't block the recording
    // from being saved and queued for transcription.
    let duration_ms = ffmpeg::probe_duration_ms(&result_path).await.ok();

    let recording_id = uuid::Uuid::new_v4().to_string();
    let job_id = {
        let recording_id = recording_id.clone();
        let video_path = result_path.clone();
        db::with_db(&state.db, move |conn| {
            db::insert_recording_with_transcribe_job(
                conn,
                &recording_id,
                &video_path,
                language_override.as_deref(),
                duration_ms,
            )
        })
        .await
        .map_err(|e| e.to_string())?
    };
    let _ = state.job_tx.send(job_id);

    Ok(StopRecordingResult {
        recording_id,
        video_path: result_path.to_string_lossy().to_string(),
    })
}

#[tauri::command]
pub async fn get_recordings_dir(state: State<'_, AppState>) -> Result<String, String> {
    Ok(state.recordings_dir.lock().await.to_string_lossy().to_string())
}

/// Opens the native folder picker and returns the chosen path, or `None` if
/// the user cancelled. Does not change anything by itself — the caller is
/// expected to follow up with `set_recordings_dir` once the user confirms.
#[tauri::command]
pub async fn pick_recordings_dir(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let state = app.state::<AppState>();
    let current_dir = state.recordings_dir.lock().await.clone();
    drop(state);
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_directory(&current_dir)
            .blocking_pick_folder()
            .map(|p| p.to_string())
    })
    .await
    .map_err(|e| e.to_string())
}

/// Changes where future recordings are saved. Rejected while a recording is
/// in progress, since that recording's working directory was already
/// created under the old folder (see `CaptureController::new`).
#[tauri::command]
pub async fn set_recordings_dir(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let guard = state.controller.lock().await;
    if guard.is_some() {
        return Err("cannot change the recordings folder while a recording is in progress".to_string());
    }
    drop(guard);

    let new_dir = PathBuf::from(&path);
    std::fs::create_dir_all(&new_dir).map_err(|e| e.to_string())?;

    db::with_db(&state.db, move |conn| db::set_setting(conn, "recordings_dir", &path))
        .await
        .map_err(|e| e.to_string())?;

    *state.recordings_dir.lock().await = new_dir;
    Ok(())
}

#[tauri::command]
pub async fn recording_status(state: State<'_, AppState>) -> Result<RecordingStatus, String> {
    let guard = state.controller.lock().await;
    Ok(guard.as_ref().map(|c| c.status()).unwrap_or(RecordingStatus::Stopped))
}

/// Shows the on-screen annotation toolbar + transparent drawing overlay
/// (rectangle/oval/line highlighting), so it can be shown/hidden
/// independently of whether a recording is active.
#[tauri::command]
pub async fn show_annotation_overlay(app: tauri::AppHandle) -> Result<(), String> {
    overlay::show(&app).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn hide_annotation_overlay(app: tauri::AppHandle) -> Result<(), String> {
    overlay::hide(&app).map_err(|e| e.to_string())
}

/// The user-configurable OS-wide hotkey that opens/closes the annotation
/// overlay (see `hotkeys::apply`) — read at startup and whenever the setting
/// changes, same global-KV-setting shape as `get_stt_settings`.
#[tauri::command]
pub async fn get_hotkey_settings(state: State<'_, AppState>) -> Result<HotkeySettings, String> {
    db::with_db(&state.db, |conn| hotkeys::load_settings(conn))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_hotkey_settings(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    settings: HotkeySettings,
) -> Result<(), String> {
    hotkeys::apply(&app, &state, &settings.toggle_annotation)
        .await
        .map_err(|e| e.to_string())?;
    db::with_db(&state.db, move |conn| hotkeys::save_settings(conn, &settings))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_recordings(state: State<'_, AppState>) -> Result<Vec<RecordingSummary>, String> {
    db::with_db(&state.db, db::list_recordings)
        .await
        .map_err(|e| e.to_string())
}

/// Empty/whitespace-only titles are normalized to `None` so clearing the
/// rename field falls back to the UI's default display (formatted date)
/// instead of persisting an empty string.
#[tauri::command]
pub async fn rename_recording(
    state: State<'_, AppState>,
    recording_id: String,
    title: Option<String>,
) -> Result<(), String> {
    let title = title.and_then(|t| {
        let trimmed = t.trim().to_string();
        if trimmed.is_empty() { None } else { Some(trimmed) }
    });
    db::with_db(&state.db, move |conn| {
        db::set_recording_title(conn, &recording_id, title.as_deref())
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_recording(
    state: State<'_, AppState>,
    recording_id: String,
) -> Result<(), String> {
    let video_path = db::with_db(&state.db, move |conn| db::delete_recording(conn, &recording_id))
        .await
        .map_err(|e| e.to_string())?;
    // Best-effort: the DB rows are already gone (the source of truth for
    // "is this recording deleted"), so a missing/locked folder here isn't
    // reported back as a failure.
    if let Some(folder) = video_path.parent() {
        let _ = std::fs::remove_dir_all(folder);
    }
    Ok(())
}

#[tauri::command]
pub async fn search_recordings(
    state: State<'_, AppState>,
    query: String,
) -> Result<Vec<db::SearchResult>, String> {
    db::with_db(&state.db, move |conn| db::search_recordings(conn, &query))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_transcript(
    state: State<'_, AppState>,
    recording_id: String,
) -> Result<Vec<Segment>, String> {
    db::with_db(&state.db, move |conn| {
        db::get_transcript_segments(conn, &recording_id)
    })
    .await
    .map_err(|e| e.to_string())
}

/// `export_subtitles`'s response. A `Vec` rather than a single path because
/// F9's "export as separate files" mode produces two files
/// (`original.<lang>.srt` + `translated.<lang>.srt`) from one call.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSubtitlesResult {
    pub paths: Vec<String>,
}

fn render_subtitles(format: &str, segments: &[Segment]) -> String {
    if format == "srt" {
        subtitle::segments_to_srt(segments)
    } else {
        subtitle::segments_to_vtt(segments)
    }
}

fn render_dual_subtitles(format: &str, original: &[Segment], translated: &[Segment]) -> String {
    if format == "srt" {
        subtitle::dual_segments_to_srt(original, translated)
    } else {
        subtitle::dual_segments_to_vtt(original, translated)
    }
}

fn write_subtitle_file(video_path: &std::path::Path, file_name: &str, contents: String) -> Result<String, String> {
    let path = video_path
        .parent()
        .map(|dir| dir.join(file_name))
        .unwrap_or_else(|| PathBuf::from(file_name));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, contents).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().to_string())
}

/// F2/F8/F9's subtitle export, covering three modes:
/// - `"original"`: the transcript in the video's own (detected/overridden)
///   language — F2's baseline "Export subtitles."
/// - `"translated"`: the transcript translated into `language` (requires a
///   prior `translate_subtitles` call for that language — F8).
/// - `"dual"`: both together, either as one file with two lines per cue
///   (default) or, when `separate_files` is set, as
///   `original.<lang>.<format>` + `translated.<lang>.<format>` (F9).
#[tauri::command]
pub async fn export_subtitles(
    state: State<'_, AppState>,
    recording_id: String,
    format: String,
    mode: String,
    language: Option<String>,
    separate_files: bool,
) -> Result<ExportSubtitlesResult, String> {
    if format != "srt" && format != "vtt" {
        return Err(format!("unsupported subtitle format: {format}"));
    }
    if mode != "original" && mode != "translated" && mode != "dual" {
        return Err(format!("unsupported export mode: {mode}"));
    }

    let video_path = {
        let rid = recording_id.clone();
        db::with_db(&state.db, move |conn| db::get_recording_video_path(conn, &rid))
            .await
            .map_err(|e| e.to_string())?
    };

    if mode == "original" {
        let segments = db::with_db(&state.db, move |conn| {
            db::get_transcript_segments(conn, &recording_id)
        })
        .await
        .map_err(|e| e.to_string())?;
        let contents = render_subtitles(&format, &segments);
        let output_path = video_path.with_extension(&format);
        if let Some(dir) = output_path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&output_path, contents).map_err(|e| e.to_string())?;
        return Ok(ExportSubtitlesResult { paths: vec![output_path.to_string_lossy().to_string()] });
    }

    let language = language.filter(|l| !l.trim().is_empty()).ok_or_else(|| {
        "a target language is required for a translated or dual export".to_string()
    })?;
    let translated = {
        let rid = recording_id.clone();
        let lang = language.clone();
        db::with_db(&state.db, move |conn| db::get_translated_segments(conn, &rid, &lang))
            .await
            .map_err(|e| e.to_string())?
    };
    if translated.is_empty() {
        return Err("no translation available for this language yet — run Translate first".to_string());
    }

    if mode == "translated" {
        let contents = render_subtitles(&format, &translated);
        let path = write_subtitle_file(&video_path, &format!("{language}.{format}"), contents)?;
        return Ok(ExportSubtitlesResult { paths: vec![path] });
    }

    // mode == "dual"
    let original = {
        let rid = recording_id.clone();
        db::with_db(&state.db, move |conn| db::get_transcript_segments(conn, &rid))
            .await
            .map_err(|e| e.to_string())?
    };
    if separate_files {
        let original_language = {
            let rid = recording_id.clone();
            db::with_db(&state.db, move |conn| db::get_recording_summary(conn, &rid))
                .await
                .map_err(|e| e.to_string())?
                .detected_language
                .unwrap_or_else(|| "original".to_string())
        };
        let original_contents = render_subtitles(&format, &original);
        let translated_contents = render_subtitles(&format, &translated);
        let original_path = write_subtitle_file(
            &video_path,
            &format!("original.{original_language}.{format}"),
            original_contents,
        )?;
        let translated_path = write_subtitle_file(
            &video_path,
            &format!("translated.{language}.{format}"),
            translated_contents,
        )?;
        Ok(ExportSubtitlesResult { paths: vec![original_path, translated_path] })
    } else {
        let contents = render_dual_subtitles(&format, &original, &translated);
        let path = write_subtitle_file(&video_path, &format!("dual.{language}.{format}"), contents)?;
        Ok(ExportSubtitlesResult { paths: vec![path] })
    }
}

/// `get_llm_settings`'s response: provider config plus whether a cloud token
/// is saved (never the token itself — see `llm::read_cloud_token`'s doc
/// comment on why it never leaves the keychain except for a single request).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmSettingsResponse {
    #[serde(flatten)]
    pub settings: LlmSettings,
    pub cloud_token_configured: bool,
}

#[tauri::command]
pub async fn get_llm_settings(state: State<'_, AppState>) -> Result<LlmSettingsResponse, String> {
    let settings = db::with_db(&state.db, |conn| llm::load_settings(conn))
        .await
        .map_err(|e| e.to_string())?;
    let cloud_token_configured = llm::read_cloud_token()
        .map_err(|e| e.to_string())?
        .is_some();
    Ok(LlmSettingsResponse { settings, cloud_token_configured })
}

#[tauri::command]
pub async fn set_llm_settings(
    state: State<'_, AppState>,
    settings: LlmSettings,
) -> Result<(), String> {
    db::with_db(&state.db, move |conn| llm::save_settings(conn, &settings))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_cloud_api_token(token: String) -> Result<(), String> {
    llm::write_cloud_token(&token).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn clear_cloud_api_token() -> Result<(), String> {
    llm::clear_cloud_token().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn check_local_llm_connection(base_url: String) -> Result<(), String> {
    llm::check_connection(&base_url).await.map_err(|e| e.to_string())
}

/// Validates that a transcript and a usable provider both exist, then
/// enqueues a `summarize` job. Validation happens *before* touching the
/// queue so an unconfigured/invalid provider produces a clear error without
/// any transcript data ever being sent anywhere (F3's Given/When/Then for a
/// missing/invalid cloud token).
#[tauri::command]
pub async fn generate_summary(
    state: State<'_, AppState>,
    recording_id: String,
    summary_language: Option<String>,
) -> Result<(), String> {
    let segments = {
        let rid = recording_id.clone();
        db::with_db(&state.db, move |conn| db::get_transcript_segments(conn, &rid))
            .await
            .map_err(|e| e.to_string())?
    };
    if segments.is_empty() {
        return Err("no transcript available for this recording yet".to_string());
    }

    ensure_llm_provider_configured(&state).await?;

    {
        let rid = recording_id.clone();
        let lang = summary_language.clone();
        db::with_db(&state.db, move |conn| {
            db::set_recording_summary_language_override(conn, &rid, lang.as_deref())
        })
        .await
        .map_err(|e| e.to_string())?;
    }

    let job_id = {
        let rid = recording_id.clone();
        db::with_db(&state.db, move |conn| db::insert_summarize_job(conn, &rid))
            .await
            .map_err(|e| e.to_string())?
    };
    let _ = state.job_tx.send(job_id);
    Ok(())
}

/// Shared by `generate_summary` and `translate_subtitles`: both need a
/// usable LLM provider before enqueueing a job, so an unconfigured/invalid
/// provider produces a clear error without any transcript data ever being
/// sent anywhere (F3/F8's "clear error, no data sent" criteria).
async fn ensure_llm_provider_configured(state: &State<'_, AppState>) -> Result<(), String> {
    let settings = db::with_db(&state.db, |conn| llm::load_settings(conn))
        .await
        .map_err(|e| e.to_string())?;
    match settings.provider {
        ProviderKind::Cloud => {
            let has_token = llm::read_cloud_token()
                .map_err(|e| e.to_string())?
                .is_some();
            if !has_token {
                return Err("cloud provider selected but no API token is configured".to_string());
            }
        }
        ProviderKind::Local => {
            if settings.local_base_url.trim().is_empty() {
                return Err("local provider base URL is not configured".to_string());
            }
        }
    }
    Ok(())
}

/// `get_translation_state`'s response: which languages already have a
/// persisted translation, and whether a `translate` job for this recording
/// is still queued or running. The frontend calls this whenever the
/// selected recording changes so `translatedLanguages`/`translating` (pure
/// React state) get rehydrated from the DB instead of staying empty after a
/// page reload or app restart, even though the backend job queue survives
/// one (see `pipeline::recover_and_enqueue`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslationStateResult {
    pub languages: Vec<String>,
    pub in_progress: bool,
}

#[tauri::command]
pub async fn get_translation_state(
    state: State<'_, AppState>,
    recording_id: String,
) -> Result<TranslationStateResult, String> {
    let languages = {
        let rid = recording_id.clone();
        db::with_db(&state.db, move |conn| db::list_translated_languages(conn, &rid))
            .await
            .map_err(|e| e.to_string())?
    };
    let in_progress = db::with_db(&state.db, move |conn| db::has_active_translate_job(conn, &recording_id))
        .await
        .map_err(|e| e.to_string())?;
    Ok(TranslationStateResult { languages, in_progress })
}

/// Validates that a transcript and a usable provider both exist, then
/// enqueues a `translate` job (F8). Mirrors `generate_summary`'s validation
/// order for the same reason: no data goes anywhere until the provider is
/// confirmed usable.
#[tauri::command]
pub async fn translate_subtitles(
    state: State<'_, AppState>,
    recording_id: String,
    target_language: String,
) -> Result<(), String> {
    let target_language = target_language.trim().to_string();
    if target_language.is_empty() {
        return Err("a target language is required".to_string());
    }

    let segments = {
        let rid = recording_id.clone();
        db::with_db(&state.db, move |conn| db::get_transcript_segments(conn, &rid))
            .await
            .map_err(|e| e.to_string())?
    };
    if segments.is_empty() {
        return Err("no transcript available for this recording yet".to_string());
    }

    let already_active = {
        let rid = recording_id.clone();
        db::with_db(&state.db, move |conn| db::has_active_translate_job(conn, &rid))
            .await
            .map_err(|e| e.to_string())?
    };
    if already_active {
        return Err("a translation is already in progress for this recording".to_string());
    }

    ensure_llm_provider_configured(&state).await?;

    {
        let rid = recording_id.clone();
        let lang = target_language.clone();
        db::with_db(&state.db, move |conn| {
            db::set_recording_subtitle_language_override(conn, &rid, Some(&lang))
        })
        .await
        .map_err(|e| e.to_string())?;
    }

    let job_id = {
        let rid = recording_id.clone();
        db::with_db(&state.db, move |conn| db::insert_translate_job(conn, &rid))
            .await
            .map_err(|e| e.to_string())?
    };
    let _ = state.job_tx.send(job_id);
    Ok(())
}

/// Pauses an in-flight `translate` job (F8) between segments -- a no-op if
/// no `translate` job is currently running for this recording (already
/// finished, or never started), matching this app's other idempotent
/// UI-triggered commands (e.g. `clear_cloud_api_token`).
#[tauri::command]
pub async fn pause_translation(state: State<'_, AppState>, recording_id: String) -> Result<(), String> {
    let controls = state.translation_controls.lock().await;
    if let Some(control) = controls.get(&recording_id) {
        control.pause();
    }
    Ok(())
}

/// Resumes a `translate` job paused via `pause_translation`.
#[tauri::command]
pub async fn resume_translation(state: State<'_, AppState>, recording_id: String) -> Result<(), String> {
    let controls = state.translation_controls.lock().await;
    if let Some(control) = controls.get(&recording_id) {
        control.resume();
    }
    Ok(())
}

/// Cancels an in-flight `translate` job. Any segments already translated in
/// this run are discarded rather than partially persisted -- see
/// `pipeline::run_translation`'s early return on cancellation -- so the
/// recording is left exactly as if `translate_subtitles` had never been
/// called for this language, and the user can just start over.
#[tauri::command]
pub async fn cancel_translation(state: State<'_, AppState>, recording_id: String) -> Result<(), String> {
    let controls = state.translation_controls.lock().await;
    if let Some(control) = controls.get(&recording_id) {
        control.cancel();
    }
    Ok(())
}

/// The STT engine choice — Whisper (99+ languages, auto-detect) vs. NVIDIA
/// Parakeet-TDT-0.6B-v3 (faster, ~25 languages, no auto-detect) — is a global
/// setting read at `transcribe` job run time, mirroring `get_llm_settings`.
#[tauri::command]
pub async fn get_stt_settings(state: State<'_, AppState>) -> Result<SttSettings, String> {
    db::with_db(&state.db, |conn| stt::load_settings(conn))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_stt_settings(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    settings: SttSettings,
) -> Result<(), String> {
    db::with_db(&state.db, {
        let settings = settings.clone();
        move |conn| stt::save_settings(conn, &settings)
    })
    .await
    .map_err(|e| e.to_string())?;
    // The Settings window edits this independently of the main window's own
    // copy (each Tauri window is a separate webview/JS runtime) — broadcast
    // so the main window's recording-language picker stays in sync.
    app.emit("stt-settings-changed", &settings)
        .map_err(|e| e.to_string())
}

/// Opens (or refocuses) the Settings window — Speech recognition, Hotkeys,
/// and LLM provider tabs, see `SettingsWindow.tsx`.
#[tauri::command]
pub async fn open_settings_window(app: tauri::AppHandle) -> Result<(), String> {
    settings_window::show(&app).map_err(|e| e.to_string())
}

/// F6 live subtitles toggle — read at `start_recording` time (see
/// `commands::start_recording`), same global-KV-setting shape as
/// `get_stt_settings`/`set_stt_settings`.
#[tauri::command]
pub async fn get_live_subtitle_settings(
    state: State<'_, AppState>,
) -> Result<LiveSubtitlesSettings, String> {
    db::with_db(&state.db, |conn| live_subtitles::load_settings(conn))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_live_subtitle_settings(
    state: State<'_, AppState>,
    settings: LiveSubtitlesSettings,
) -> Result<(), String> {
    db::with_db(&state.db, move |conn| {
        live_subtitles::save_settings(conn, &settings)
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_summary(
    state: State<'_, AppState>,
    recording_id: String,
) -> Result<Option<Summary>, String> {
    db::with_db(&state.db, move |conn| db::get_summary(conn, &recording_id))
        .await
        .map_err(|e| e.to_string())
}
