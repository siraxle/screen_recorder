mod capture;
mod commands;
mod state;

use tauri::Manager;

use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let recordings_dir = app.path().app_data_dir()?.join("recordings");
            std::fs::create_dir_all(&recordings_dir)?;
            app.manage(AppState {
                controller: tokio::sync::Mutex::new(None),
                recordings_dir,
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
