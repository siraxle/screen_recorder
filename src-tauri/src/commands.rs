use serde::{Deserialize, Serialize};
use tauri::State;

use crate::capture::{ffmpeg, CaptureConfig, CaptureController, RecordingStatus};
use crate::db::{self, RecordingSummary, Segment};
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
    state: State<'_, AppState>,
    settings: RecordingSettings,
) -> Result<(), String> {
    let mut guard = state.controller.lock().await;
    if guard.is_some() {
        return Err("a recording is already in progress".to_string());
    }
    let mut controller = CaptureController::new(
        settings.capture,
        &state.recordings_dir,
        settings.language_override,
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

    let filename = format!(
        "recording-{}.mp4",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
    );
    let output_path = state.recordings_dir.join(filename);
    let language_override = controller.language_override().map(str::to_string);
    let result_path = controller
        .stop(&output_path)
        .await
        .map_err(|e| e.to_string())?;

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
pub async fn recording_status(state: State<'_, AppState>) -> Result<RecordingStatus, String> {
    let guard = state.controller.lock().await;
    Ok(guard.as_ref().map(|c| c.status()).unwrap_or(RecordingStatus::Stopped))
}

#[tauri::command]
pub async fn list_recordings(state: State<'_, AppState>) -> Result<Vec<RecordingSummary>, String> {
    db::with_db(&state.db, db::list_recordings)
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

#[tauri::command]
pub async fn export_subtitles(
    state: State<'_, AppState>,
    recording_id: String,
    format: String,
) -> Result<String, String> {
    if format != "srt" && format != "vtt" {
        return Err(format!("unsupported subtitle format: {format}"));
    }

    let video_path = {
        let recording_id = recording_id.clone();
        db::with_db(&state.db, move |conn| {
            db::get_recording_video_path(conn, &recording_id)
        })
        .await
        .map_err(|e| e.to_string())?
    };
    let segments = db::with_db(&state.db, move |conn| {
        db::get_transcript_segments(conn, &recording_id)
    })
    .await
    .map_err(|e| e.to_string())?;

    let contents = if format == "srt" {
        subtitle::segments_to_srt(&segments)
    } else {
        subtitle::segments_to_vtt(&segments)
    };

    let output_path = video_path.with_extension(&format);
    std::fs::write(&output_path, contents).map_err(|e| e.to_string())?;
    Ok(output_path.to_string_lossy().to_string())
}
