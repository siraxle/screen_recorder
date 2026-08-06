//! Persisted transcription job queue (F2). See CLAUDE.md: pipeline stages
//! must be explicit and persisted so a crash mid-pipeline resumes from the
//! last completed stage rather than from scratch.

pub mod stt;

use anyhow::Result;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

use crate::capture::ffmpeg;
use crate::db::{self, PipelineStatus, SharedConn};

pub type JobSender = mpsc::UnboundedSender<String>;

/// Spawns the single background worker that drains the job queue. One
/// worker, sequential processing: there is one GPU and one model instance,
/// so no concurrency is needed at this scope.
pub fn spawn_queue_worker(db: SharedConn, app_handle: AppHandle) -> JobSender {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    // `tauri::async_runtime::spawn`, not `tokio::spawn`: this is called
    // synchronously from the (non-async) `.setup()` closure in lib.rs,
    // before any tokio reactor is entered on this thread. Tauri's spawn
    // dispatches onto its managed runtime regardless of the caller's
    // context.
    tauri::async_runtime::spawn(async move {
        while let Some(job_id) = rx.recv().await {
            if let Err(e) = transcribe_one(&db, &app_handle, &job_id).await {
                tracing::error!("transcription job {job_id} failed: {e:#}");
            }
        }
    });
    tx
}

/// Resets any job left `running` by a previous crashed run and re-enqueues
/// every pending job. Must run once at startup, after the DB is open and
/// before anything else touches the queue.
pub async fn recover_and_enqueue(db: &SharedConn, tx: &JobSender) -> Result<()> {
    let ids = db::with_db(db, db::recover_pending_job_ids).await?;
    for id in ids {
        let _ = tx.send(id);
    }
    Ok(())
}

async fn transcribe_one(db: &SharedConn, app_handle: &AppHandle, job_id: &str) -> Result<()> {
    let job = {
        let job_id = job_id.to_string();
        db::with_db(db, move |conn| db::get_job(conn, &job_id)).await?
    };
    // Guards against a race between startup recovery and a live enqueue for
    // the same job id.
    if job.status != "pending" {
        return Ok(());
    }
    let recording_id = job.recording_id.clone();

    {
        let job_id = job.id.clone();
        db::with_db(db, move |conn| db::set_job_running(conn, &job_id)).await?;
    }
    {
        let rid = recording_id.clone();
        db::with_db(db, move |conn| {
            db::set_recording_status(conn, &rid, PipelineStatus::Transcribing)
        })
        .await?;
    }
    emit_update(app_handle, db, &recording_id).await;

    match run_transcription(db, &recording_id).await {
        Ok(()) => {
            let job_id = job.id.clone();
            db::with_db(db, move |conn| db::set_job_done(conn, &job_id)).await?;
        }
        Err(e) => {
            let message = format!("{e:#}");
            let job_id = job.id.clone();
            db::with_db(db, move |conn| db::set_job_failed(conn, &job_id, &message)).await?;
            let rid = recording_id.clone();
            db::with_db(db, move |conn| {
                db::set_recording_status(conn, &rid, PipelineStatus::Failed)
            })
            .await?;
        }
    }
    emit_update(app_handle, db, &recording_id).await;
    Ok(())
}

async fn run_transcription(db: &SharedConn, recording_id: &str) -> Result<()> {
    let video_path = {
        let rid = recording_id.to_string();
        db::with_db(db, move |conn| db::get_recording_video_path(conn, &rid)).await?
    };
    let language_override = {
        let rid = recording_id.to_string();
        db::with_db(db, move |conn| db::get_recording_language_override(conn, &rid)).await?
    };

    let wav_path = video_path.with_extension("stt.wav");
    ffmpeg::run_to_completion(
        ffmpeg::build_wav_extract_command(&video_path, &wav_path),
        "wav-extract",
    )
    .await?;

    let result = stt::transcribe(&wav_path, language_override.as_deref()).await;
    let _ = std::fs::remove_file(&wav_path);
    let result = result?;

    {
        let rid = recording_id.to_string();
        let segments = result.segments;
        db::with_db(db, move |conn| {
            db::replace_transcript_segments(conn, &rid, &segments)
        })
        .await?;
    }
    {
        let rid = recording_id.to_string();
        let language = result.language;
        db::with_db(db, move |conn| {
            db::set_recording_detected_language(conn, &rid, &language)
        })
        .await?;
    }
    {
        let rid = recording_id.to_string();
        db::with_db(db, move |conn| {
            db::set_recording_status(conn, &rid, PipelineStatus::Transcribed)
        })
        .await?;
    }
    Ok(())
}

async fn emit_update(app_handle: &AppHandle, db: &SharedConn, recording_id: &str) {
    let rid = recording_id.to_string();
    if let Ok(summary) = db::with_db(db, move |conn| db::get_recording_summary(conn, &rid)).await {
        let _ = app_handle.emit("recording-updated", summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Real (non-mocked) integration test of the queue-independent part of
    /// the pipeline (db + ffmpeg wav extraction + the real whisper worker):
    /// inserts a recording row pointing at the checked-in speech fixture,
    /// runs `run_transcription` directly, and asserts the DB ends up in the
    /// state F2's Given/When/Then #1 requires (status "transcribed", a
    /// non-empty transcript, and the detected language recorded).
    #[tokio::test]
    async fn run_transcription_persists_transcript_and_status() {
        let base_dir = std::env::temp_dir().join("screen_recorder_pipeline_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();

        let db = db::open(&base_dir.join("db.sqlite3")).unwrap();

        // ffmpeg only needs a decodable audio stream to extract from, so the
        // checked-in WAV fixture stands in for a real recorded mp4 here —
        // capture itself is already covered by `capture::controller`'s test.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("sample_en.wav");
        let video_path = base_dir.join("fake_recording.wav");
        std::fs::copy(&fixture, &video_path).unwrap();

        let recording_id = uuid::Uuid::new_v4().to_string();
        {
            let rid = recording_id.clone();
            let vp = video_path.clone();
            db::with_db(&db, move |conn| {
                db::insert_recording_with_transcribe_job(conn, &rid, &vp, None)
            })
            .await
            .unwrap();
        }

        run_transcription(&db, &recording_id)
            .await
            .expect("pipeline should succeed");

        let summary = {
            let rid = recording_id.clone();
            db::with_db(&db, move |conn| db::get_recording_summary(conn, &rid))
                .await
                .unwrap()
        };
        assert_eq!(summary.status, PipelineStatus::Transcribed);
        assert_eq!(summary.detected_language.as_deref(), Some("en"));

        let segments = db::with_db(&db, move |conn| {
            db::get_transcript_segments(conn, &recording_id)
        })
        .await
        .unwrap();
        assert!(!segments.is_empty(), "expected persisted transcript segments");
    }
}
