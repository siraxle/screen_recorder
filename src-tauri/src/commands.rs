use tauri::State;

use crate::capture::{ffmpeg, CaptureConfig, CaptureController, RecordingStatus};
use crate::state::AppState;

#[tauri::command]
pub async fn list_microphones() -> Result<Vec<String>, String> {
    ffmpeg::list_microphones().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn start_recording(
    state: State<'_, AppState>,
    config: CaptureConfig,
) -> Result<(), String> {
    let mut guard = state.controller.lock().await;
    if guard.is_some() {
        return Err("a recording is already in progress".to_string());
    }
    let mut controller =
        CaptureController::new(config, &state.recordings_dir).map_err(|e| e.to_string())?;
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

#[tauri::command]
pub async fn stop_recording(state: State<'_, AppState>) -> Result<String, String> {
    let mut guard = state.controller.lock().await;
    let mut controller = guard.take().ok_or("no recording in progress")?;
    drop(guard);

    let filename = format!(
        "recording-{}.mp4",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
    );
    let output_path = state.recordings_dir.join(filename);
    let result_path = controller
        .stop(&output_path)
        .await
        .map_err(|e| e.to_string())?;
    Ok(result_path.to_string_lossy().to_string())
}

#[tauri::command]
pub async fn recording_status(state: State<'_, AppState>) -> Result<RecordingStatus, String> {
    let guard = state.controller.lock().await;
    Ok(guard.as_ref().map(|c| c.status()).unwrap_or(RecordingStatus::Stopped))
}
