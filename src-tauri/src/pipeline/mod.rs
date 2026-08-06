//! Persisted transcription job queue (F2). See CLAUDE.md: pipeline stages
//! must be explicit and persisted so a crash mid-pipeline resumes from the
//! last completed stage rather than from scratch.

pub mod stt;

use anyhow::Result;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

use crate::capture::ffmpeg;
use crate::db::{self, PipelineStatus, SharedConn, Summary};
use crate::llm;

pub type JobSender = mpsc::UnboundedSender<String>;

/// Payload for the `transcription-progress` event, emitted as
/// `pipeline::stt::transcribe` reports progress fractions. Pushed
/// independently of `recording-updated` (which only fires on stage
/// transitions) since progress changes far more often than the DB-backed
/// summary needs to be re-read for.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptionProgress {
    recording_id: String,
    /// 0.0..=1.0
    progress: f64,
}

/// Payload for the `job-failed` event -- surfaces an async job's error to
/// the UI even when the recording's persisted status doesn't change (e.g. a
/// failed `summarize` job reverts to `transcribed` rather than `failed`, see
/// `process_job`, so the error would otherwise be silently lost).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JobFailed {
    recording_id: String,
    stage: String,
    message: String,
}

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
            if let Err(e) = process_job(&db, &app_handle, &job_id).await {
                tracing::error!("job {job_id} failed: {e:#}");
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

/// Dispatches a queued job to its stage's handler. Both stages share the
/// same running/done/failed job bookkeeping and `recording-updated`
/// emission; only the work in between (and the in-progress/final
/// `PipelineStatus`) differs per stage.
async fn process_job(db: &SharedConn, app_handle: &AppHandle, job_id: &str) -> Result<()> {
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

    let in_progress_status = match job.stage.as_str() {
        "summarize" => PipelineStatus::Summarizing,
        _ => PipelineStatus::Transcribing,
    };

    {
        let job_id = job.id.clone();
        db::with_db(db, move |conn| db::set_job_running(conn, &job_id)).await?;
    }
    {
        let rid = recording_id.clone();
        db::with_db(db, move |conn| db::set_recording_status(conn, &rid, in_progress_status))
            .await?;
    }
    emit_update(app_handle, db, &recording_id).await;

    let result = match job.stage.as_str() {
        "summarize" => run_summarization(db, &recording_id).await,
        _ => {
            let progress_handle = app_handle.clone();
            let progress_recording_id = recording_id.clone();
            let on_progress = move |progress: f64| {
                let _ = progress_handle.emit(
                    "transcription-progress",
                    TranscriptionProgress {
                        recording_id: progress_recording_id.clone(),
                        progress,
                    },
                );
            };
            run_transcription(db, &recording_id, on_progress).await
        }
    };

    match result {
        Ok(()) => {
            let job_id = job.id.clone();
            db::with_db(db, move |conn| db::set_job_done(conn, &job_id)).await?;
        }
        Err(e) => {
            let message = format!("{e:#}");
            let job_id = job.id.clone();
            let stored_message = message.clone();
            db::with_db(db, move |conn| db::set_job_failed(conn, &job_id, &stored_message)).await?;
            // A failed `summarize` job leaves a perfectly good transcript in
            // place (F3 is a user-triggered step layered on top of F2, per
            // CLAUDE.md's job-queue stages) -- only `transcribe` failing
            // means there's no usable transcript, so only that stage should
            // stamp the recording as `failed`.
            let revert_status = match job.stage.as_str() {
                "summarize" => PipelineStatus::Transcribed,
                _ => PipelineStatus::Failed,
            };
            let rid = recording_id.clone();
            db::with_db(db, move |conn| {
                db::set_recording_status(conn, &rid, revert_status)
            })
            .await?;
            let _ = app_handle.emit(
                "job-failed",
                JobFailed {
                    recording_id: recording_id.clone(),
                    stage: job.stage.clone(),
                    message,
                },
            );
        }
    }
    emit_update(app_handle, db, &recording_id).await;
    Ok(())
}

async fn run_transcription(
    db: &SharedConn,
    recording_id: &str,
    on_progress: impl Fn(f64) + Send + 'static,
) -> Result<()> {
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

    let result = stt::transcribe(&wav_path, language_override.as_deref(), on_progress).await;
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

/// F3: builds a prompt from the persisted transcript, asks the configured
/// LLM provider for a JSON summary, and persists the result. Target
/// language is the user's explicit summary-language override if set,
/// otherwise the transcript's detected language (spec's language default).
async fn run_summarization(db: &SharedConn, recording_id: &str) -> Result<()> {
    let segments = {
        let rid = recording_id.to_string();
        db::with_db(db, move |conn| db::get_transcript_segments(conn, &rid)).await?
    };
    if segments.is_empty() {
        return Err(anyhow::anyhow!("no transcript available to summarize"));
    }

    let detected_language = {
        let rid = recording_id.to_string();
        db::with_db(db, move |conn| db::get_recording_summary(conn, &rid))
            .await?
            .detected_language
    };
    let language_override = {
        let rid = recording_id.to_string();
        db::with_db(db, move |conn| {
            db::get_recording_summary_language_override(conn, &rid)
        })
        .await?
    };
    let target_language = language_override
        .or(detected_language)
        .unwrap_or_else(|| "en".to_string());

    let settings = db::with_db(db, |conn| llm::load_settings(conn)).await?;
    let cloud_token = llm::read_cloud_token()?;

    let transcript_text: String = segments
        .iter()
        .map(|s| s.text.trim())
        .collect::<Vec<_>>()
        .join(" ");
    let system_prompt = format!(
        "You summarize meeting/lecture transcripts. Respond ONLY with a JSON \
         object of the shape {{\"overview\": string, \"key_points\": string[]}} \
         — a short overview plus a list of key points. Write the summary in \
         the language with ISO 639-1 code \"{target_language}\", regardless \
         of the transcript's language."
    );

    let reply =
        llm::complete(&settings, cloud_token.as_deref(), &system_prompt, &transcript_text).await?;
    let parsed = llm::parse_summary_reply(&reply);

    let summary = Summary {
        overview: parsed.overview,
        key_points: parsed.key_points,
        language: target_language,
    };
    {
        let rid = recording_id.to_string();
        db::with_db(db, move |conn| db::replace_summary(conn, &rid, &summary)).await?;
    }
    {
        let rid = recording_id.to_string();
        db::with_db(db, move |conn| {
            db::set_recording_status(conn, &rid, PipelineStatus::Summarized)
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
                db::insert_recording_with_transcribe_job(conn, &rid, &vp, None, None)
            })
            .await
            .unwrap();
        }

        run_transcription(&db, &recording_id, |_| {})
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

    /// Real DB + real HTTP round trip against a local `wiremock` server
    /// standing in for an OpenAI-compatible local LLM endpoint (a real
    /// external LLM isn't available in CI, so only that boundary is faked —
    /// everything else, including JSON parsing and persistence, is real).
    /// Asserts F3's Given/When/Then: a ready transcript plus a configured
    /// provider yields a persisted overview + key points and status
    /// "summarized".
    #[tokio::test]
    async fn run_summarization_persists_summary_and_status() {
        let base_dir = std::env::temp_dir().join("screen_recorder_summarize_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();
        let db = db::open(&base_dir.join("db.sqlite3")).unwrap();

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content":
                    "{\"overview\": \"a test overview\", \"key_points\": [\"point one\"]}"
                }}]
            })))
            .mount(&server)
            .await;

        let recording_id = uuid::Uuid::new_v4().to_string();
        let video_path = base_dir.join("fake_recording.mp4");
        {
            let rid = recording_id.clone();
            let vp = video_path.clone();
            db::with_db(&db, move |conn| {
                db::insert_recording_with_transcribe_job(conn, &rid, &vp, None, None)
            })
            .await
            .unwrap();
        }
        {
            let rid = recording_id.clone();
            db::with_db(&db, move |conn| {
                db::replace_transcript_segments(
                    conn,
                    &rid,
                    &[db::Segment { start_ms: 0, end_ms: 1000, text: "hello world".to_string() }],
                )
            })
            .await
            .unwrap();
        }
        {
            let rid = recording_id.clone();
            db::with_db(&db, move |conn| db::set_recording_detected_language(conn, &rid, "en"))
                .await
                .unwrap();
        }
        {
            let server_uri = server.uri();
            db::with_db(&db, move |conn| {
                let settings = llm::LlmSettings {
                    provider: llm::ProviderKind::Local,
                    local_base_url: server_uri,
                    local_model: "test-model".to_string(),
                    ..llm::LlmSettings::default()
                };
                llm::save_settings(conn, &settings)
            })
            .await
            .unwrap();
        }

        run_summarization(&db, &recording_id)
            .await
            .expect("summarization should succeed");

        let recording_summary = {
            let rid = recording_id.clone();
            db::with_db(&db, move |conn| db::get_recording_summary(conn, &rid))
                .await
                .unwrap()
        };
        assert_eq!(recording_summary.status, PipelineStatus::Summarized);

        let summary = {
            let rid = recording_id.clone();
            db::with_db(&db, move |conn| db::get_summary(conn, &rid))
                .await
                .unwrap()
        }
        .expect("summary should be persisted");
        assert_eq!(summary.overview, "a test overview");
        assert_eq!(summary.key_points, vec!["point one".to_string()]);
        assert_eq!(summary.language, "en");
    }
}
