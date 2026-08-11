mod capture;
mod commands;
mod compress;
mod db;
mod hotkeys;
mod llm;
mod logging;
mod overlay;
mod pipeline;
mod portable;
mod settings_window;
mod setup;
mod state;
mod subtitle;
mod thermal;

use tauri::Manager;
use tauri_plugin_global_shortcut::ShortcutState;

use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    // Only one custom global shortcut is ever registered at a
                    // time (the annotation toggle — see `hotkeys::apply`), so
                    // any press unambiguously means "toggle the overlay."
                    if event.state() == ShortcutState::Pressed {
                        if let Err(e) = overlay::toggle(app) {
                            tracing::error!("failed to toggle annotation overlay via hotkey: {e:#}");
                        }
                    }
                })
                .build(),
        )
        .setup(|app| {
            let app_data_dir = portable::app_data_dir(app)?;
            logging::init(&app_data_dir);
            tracing::info!("app starting, data dir: {}", app_data_dir.display());

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

            // Cache the resolved interpreter/ffmpeg paths once, up front --
            // `setup::paths::python_program`/`ffmpeg_program`/`ffprobe_program`
            // fall back to the bare PATH-relative literal until this runs.
            // Resolves to a saved system/custom override (see `setup::overrides`)
            // if one is set and still valid, else the bundled `runtime/`
            // path, regardless of whether that component has actually been
            // installed yet (a job spawned before install just fails clearly,
            // same as today's "not on PATH" failure mode). `apply_override`/
            // `clear_override` re-run this same resolution at runtime when
            // the user changes their choice from the Dependencies panel.
            setup::resolve_startup_paths(&app_data_dir);
            let priming_data_dir = app_data_dir.clone();
            tauri::async_runtime::spawn(async move {
                let report = setup::check_all(&priming_data_dir).await;
                if !report.required_satisfied() {
                    tracing::info!("required runtime dependencies not yet installed -- the frontend will prompt for setup");
                }
            });

            let hotkey_settings = {
                let conn = db.lock().expect("db mutex poisoned");
                hotkeys::load_settings(&conn)?
            };

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
                live_subtitles: tokio::sync::Mutex::new(None),
                recordings_dir: tokio::sync::Mutex::new(recordings_dir),
                db,
                job_tx,
                translation_controls: tokio::sync::Mutex::new(std::collections::HashMap::new()),
                active_hotkey: tokio::sync::Mutex::new(None),
                compressing: tokio::sync::Mutex::new(std::collections::HashSet::new()),
                installing: tokio::sync::Mutex::new(false),
            });

            // Registered after `manage` since `hotkeys::apply` needs
            // `AppState.active_hotkey`; a bad or already-claimed accelerator
            // saved from a previous run must not stop the app from starting.
            let hotkey_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = hotkey_app.state::<AppState>();
                if let Err(e) = hotkeys::apply(&hotkey_app, &state, &hotkey_settings.toggle_annotation).await {
                    tracing::warn!(
                        "failed to register annotation hotkey \"{}\": {e:#}",
                        hotkey_settings.toggle_annotation
                    );
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_microphones,
            commands::list_cameras,
            commands::start_recording,
            commands::pause_recording,
            commands::resume_recording,
            commands::stop_recording,
            commands::pick_import_file,
            commands::import_recording,
            commands::recording_status,
            commands::list_recordings,
            commands::rename_recording,
            commands::delete_recording,
            commands::search_recordings,
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
            commands::translate_subtitles,
            commands::get_translation_state,
            commands::pause_translation,
            commands::resume_translation,
            commands::cancel_translation,
            commands::get_stt_settings,
            commands::set_stt_settings,
            commands::get_live_subtitle_settings,
            commands::set_live_subtitle_settings,
            commands::show_annotation_overlay,
            commands::hide_annotation_overlay,
            commands::get_hotkey_settings,
            commands::set_hotkey_settings,
            commands::open_settings_window,
            commands::compress_recording,
            commands::check_dependencies,
            commands::install_dependencies,
            commands::scan_dependencies,
            commands::pick_dependency_folder,
            commands::use_dependency_path,
            commands::clear_dependency_override,
            commands::download_log,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
