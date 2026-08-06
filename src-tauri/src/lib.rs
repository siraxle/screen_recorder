mod capture;
mod commands;
mod db;
mod llm;
mod pipeline;
mod state;
mod subtitle;

use tauri::Manager;

use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let app_data_dir = app.path().app_data_dir()?;
            let db_path = app_data_dir.join("db.sqlite3");
            let db = db::open(&db_path).expect("failed to open recordings database");

            // A user-chosen folder from a previous run (see
            // `commands::set_recordings_dir`) takes precedence over the
            // default app-data location.
            let saved_recordings_dir = {
                let conn = db.lock().expect("db mutex poisoned");
                db::get_setting(&conn, "recordings_dir")?
            };
            let recordings_dir = saved_recordings_dir
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| app_data_dir.join("recordings"));
            std::fs::create_dir_all(&recordings_dir)?;

            let job_tx = pipeline::spawn_queue_worker(db.clone(), app.handle().clone());

            let recovery_db = db.clone();
            let recovery_tx = job_tx.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = pipeline::recover_and_enqueue(&recovery_db, &recovery_tx).await {
                    tracing::error!("failed to recover pending transcription jobs: {e:#}");
                }
            });

            app.manage(AppState {
                controller: tokio::sync::Mutex::new(None),
                recordings_dir: tokio::sync::Mutex::new(recordings_dir),
                db,
                job_tx,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_microphones,
            commands::start_recording,
            commands::pause_recording,
            commands::resume_recording,
            commands::stop_recording,
            commands::recording_status,
            commands::list_recordings,
            commands::get_transcript,
            commands::export_subtitles,
            commands::get_recordings_dir,
            commands::set_recordings_dir,
            commands::pick_recordings_dir,
            commands::get_llm_settings,
            commands::set_llm_settings,
            commands::set_cloud_api_token,
            commands::clear_cloud_api_token,
            commands::check_local_llm_connection,
            commands::generate_summary,
            commands::get_summary,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
