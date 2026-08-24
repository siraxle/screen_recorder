//! Persisted transcription job queue (F2). See CLAUDE.md: pipeline stages
//! must be explicit and persisted so a crash mid-pipeline resumes from the
//! last completed stage rather than from scratch.

pub mod live_subtitles;
pub mod stt;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

use crate::capture::ffmpeg;
use crate::db::{self, PipelineStatus, SharedConn, Summary};
use crate::llm;
use crate::state::AppState;

pub type JobSender = mpsc::UnboundedSender<String>;

/// Lets the UI pause/resume or cancel an in-flight `translate` job (F8) from
/// outside the job queue -- see `commands::pause_translation`,
/// `resume_translation`, `cancel_translation`. Registered in
/// `AppState::translation_controls`, keyed by recording id, for the
/// duration of `process_translate_job`; at most one is ever actually
/// driving an LLM call at a time (`spawn_queue_worker`'s single-worker
/// queue), but keying by recording id rather than a single global slot
/// keeps a stale pause/cancel click from ever hitting the wrong job.
#[derive(Default)]
pub struct TranslationControl {
    paused: AtomicBool,
    cancelled: AtomicBool,
}

impl TranslationControl {
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Polls every 300ms while paused. A plain poll rather than a
    /// `Notify`-based wakeup: `pause`/`resume`/`cancel` come from a Tauri
    /// command handler that has no reference to whatever waiter this task
    /// may or may not currently have registered, so a `Notify::notify_*`
    /// call could easily land in the gap between this loop checking
    /// `paused` and starting to wait, and be lost -- a 300ms poll can't
    /// miss a flag flip, and is cheap enough for a UI-driven pause.
    async fn wait_if_paused(&self) {
        while self.paused.load(Ordering::SeqCst) && !self.cancelled.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
}

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

/// Payload for the `translation-updated` event (F8). A `translate` job never
/// touches `recordings.status` (see `process_translate_job`'s doc comment),
/// so unlike `transcribe`/`summarize` there's no `recording-updated` event to
/// piggyback progress on -- the UI needs its own signal to know when a
/// translation finished and whether any segments fell back to the original
/// text.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TranslationUpdate {
    recording_id: String,
    language: String,
    status: &'static str, // "done" | "failed"
    failed_segments: usize,
    total_segments: usize,
    message: Option<String>,
}

/// Emitted while a `translate`/`summarize` job is paused by
/// `thermal::wait_for_safe_temp`, so the UI can show "GPU cooling down"
/// instead of the job looking hung.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThermalCooldown {
    recording_id: String,
    gpu_temp_c: u32,
}

/// Builds the `on_cooldown` callback `run_translation`/`run_summarization`
/// call while paused for heat -- see `thermal::wait_for_safe_temp`.
fn make_cooldown_emitter(
    app_handle: &AppHandle,
    recording_id: &str,
) -> impl Fn(u32) + Send + Sync {
    let app_handle = app_handle.clone();
    let recording_id = recording_id.to_string();
    move |gpu_temp_c: u32| {
        let _ = app_handle.emit(
            "thermal-cooldown",
            ThermalCooldown { recording_id: recording_id.clone(), gpu_temp_c },
        );
    }
}

/// Payload for the `translation-progress` event: `run_translation` has no
/// per-stage DB status to piggyback on (see `TranslationUpdate`'s doc
/// comment), and segment-by-segment LLM calls can take long enough --
/// especially with `thermal::wait_for_safe_temp` pausing them -- that the UI
/// needs its own sense of "how far in" beyond a bare "Translating..." label.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TranslationProgress {
    recording_id: String,
    /// 0.0..=1.0
    progress: f64,
}

/// Builds the `on_progress` callback `run_translation` calls after each
/// segment.
fn make_translation_progress_emitter(
    app_handle: &AppHandle,
    recording_id: &str,
) -> impl Fn(f64) + Send + Sync {
    let app_handle = app_handle.clone();
    let recording_id = recording_id.to_string();
    move |progress: f64| {
        let _ = app_handle.emit(
            "translation-progress",
            TranslationProgress { recording_id: recording_id.clone(), progress },
        );
    }
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

    // `translate` is handled entirely separately: it must never write
    // `recordings.status` (see `process_translate_job`'s doc comment on why
    // a single status column can't represent it running concurrently with
    // `summarize`), so it doesn't fit the status-transition bookkeeping the
    // rest of this function does for `transcribe`/`summarize`.
    if job.stage == "translate" {
        return process_translate_job(db, app_handle, &job).await;
    }

    let recording_id = job.recording_id.clone();

    let in_progress_status = match job.stage.as_str() {
        "summarize" => PipelineStatus::Summarizing,
        _ => PipelineStatus::Transcribing,
    };

    tracing::info!("job {} ({}) starting for recording {recording_id}", job.id, job.stage);

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
        "summarize" => {
            run_summarization(db, &recording_id, &make_cooldown_emitter(app_handle, &recording_id))
                .await
        }
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
            tracing::info!("job {} ({}) completed for recording {recording_id}", job.id, job.stage);
            let job_id = job.id.clone();
            db::with_db(db, move |conn| db::set_job_done(conn, &job_id)).await?;
        }
        Err(e) => {
            let message = format!("{e:#}");
            tracing::error!("job {} ({}) failed for recording {recording_id}: {message}", job.id, job.stage);
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
    let stt_settings = db::with_db(db, |conn| stt::load_settings(conn)).await?;

    let wav_path = video_path.with_extension("stt.wav");
    ffmpeg::run_to_completion(
        ffmpeg::build_wav_extract_command(&video_path, &wav_path),
        "wav-extract",
    )
    .await?;

    let result = stt::transcribe(
        &wav_path,
        language_override.as_deref(),
        stt_settings.engine,
        stt_settings.device,
        on_progress,
    )
    .await;
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
async fn run_summarization(
    db: &SharedConn,
    recording_id: &str,
    on_cooldown: &(dyn Fn(u32) + Send + Sync),
) -> Result<()> {
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
    let language_name = llm::language_display_name(&target_language);
    let system_prompt = format!(
        "You summarize meeting/lecture transcripts. Respond ONLY with a JSON \
         object of the shape {{\"overview\": string, \"key_points\": string[]}}. \
         Be thorough and complete rather than brief: \
         \"overview\" should be several sentences (a short paragraph, not one \
         or two lines) that walks through the topics discussed in the order \
         they came up, including relevant context, reasoning, and any \
         conclusions reached — not just a one-line gist. \"key_points\" should \
         enumerate every distinct decision, action item, open question, and \
         noteworthy fact from the transcript as its own entry — do not \
         collapse multiple points into one entry, and do not cap the list to \
         a small handful if the transcript covers more ground than that. \
         Prefer including a point over omitting it; only leave out filler \
         and small talk that carries no information. \
         IMPORTANT: write the ENTIRE reply — the \"overview\" text and every \
         \"key_points\" entry — in {language_name} (ISO 639-1: \"{target_language}\"). \
         Do this even though the transcript below is in a different language, \
         and even if that means translating rather than just summarizing in \
         the original language. Never answer in the transcript's own language \
         unless it happens to be {language_name}."
    );
    let transcript_text = format!(
        "{transcript_text}\n\n(Reminder: respond only in {language_name}.)"
    );

    if settings.provider == llm::ProviderKind::Local {
        crate::thermal::wait_for_safe_temp(on_cooldown).await;
    }
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

/// Runs a `translate` job (F8) end to end: job bookkeeping, translation,
/// and the `translation-updated` event -- but deliberately never touches
/// `recordings.status`. Per CLAUDE.md, `translating` must be able to run
/// concurrently with `summarizing`, and `recordings.status` is a single
/// denormalized column that can't represent two stages in flight at once
/// (see the "source of truth" note atop `db.rs`), so this stage is tracked
/// only through the `jobs` table and this dedicated event.
async fn process_translate_job(
    db: &SharedConn,
    app_handle: &AppHandle,
    job: &db::JobRow,
) -> Result<()> {
    tracing::info!("job {} (translate) starting for recording {}", job.id, job.recording_id);

    {
        let job_id = job.id.clone();
        db::with_db(db, move |conn| db::set_job_running(conn, &job_id)).await?;
    }

    let recording_id = job.recording_id.clone();
    let target_language = {
        let rid = recording_id.clone();
        db::with_db(db, move |conn| db::get_recording_subtitle_language_override(conn, &rid))
            .await?
            .ok_or_else(|| anyhow::anyhow!("no subtitle language set for translation"))?
    };

    // Registered for the duration of this job so `commands::pause_translation`
    // /`resume_translation`/`cancel_translation` can reach it -- removed in
    // the `finally`-style cleanup below regardless of outcome, so a stale
    // entry never lingers for a job that already finished.
    let control = std::sync::Arc::new(TranslationControl::default());
    {
        let state = app_handle.state::<AppState>();
        let mut controls = state.translation_controls.lock().await;
        controls.insert(recording_id.clone(), control.clone());
    }

    let outcome = run_translation(
        db,
        &recording_id,
        &target_language,
        &make_cooldown_emitter(app_handle, &recording_id),
        &make_translation_progress_emitter(app_handle, &recording_id),
        &control,
    )
    .await;

    {
        let state = app_handle.state::<AppState>();
        let mut controls = state.translation_controls.lock().await;
        controls.remove(&recording_id);
    }

    let event = match &outcome {
        Ok(o) if o.cancelled => {
            let job_id = job.id.clone();
            db::with_db(db, move |conn| {
                db::set_job_failed(conn, &job_id, "cancelled by user")
            })
            .await?;
            TranslationUpdate {
                recording_id: recording_id.clone(),
                language: target_language.clone(),
                status: "cancelled",
                failed_segments: 0,
                total_segments: 0,
                message: None,
            }
        }
        Ok(o) => {
            tracing::info!(
                "job {} (translate) completed for recording {recording_id} ({}/{} segments failed)",
                job.id, o.failed_segments, o.total_segments
            );
            let job_id = job.id.clone();
            db::with_db(db, move |conn| db::set_job_done(conn, &job_id)).await?;
            TranslationUpdate {
                recording_id: recording_id.clone(),
                language: target_language.clone(),
                status: "done",
                failed_segments: o.failed_segments,
                total_segments: o.total_segments,
                message: None,
            }
        }
        Err(e) => {
            let message = format!("{e:#}");
            tracing::error!("job {} (translate) failed for recording {recording_id}: {message}", job.id);
            let job_id = job.id.clone();
            let stored_message = message.clone();
            db::with_db(db, move |conn| db::set_job_failed(conn, &job_id, &stored_message)).await?;
            TranslationUpdate {
                recording_id: recording_id.clone(),
                language: target_language.clone(),
                status: "failed",
                failed_segments: 0,
                total_segments: 0,
                message: Some(message),
            }
        }
    };
    let _ = app_handle.emit("translation-updated", event);
    Ok(())
}

struct TranslationOutcome {
    failed_segments: usize,
    total_segments: usize,
    /// Set when `TranslationControl::cancel` fired mid-loop -- see
    /// `run_translation`'s early return, which discards any segments
    /// translated so far rather than persisting a partial result.
    cancelled: bool,
}

/// F8: translates the persisted transcript segment-by-segment, preserving
/// each segment's timecodes (CLAUDE.md's subtitle-translation convention).
/// A segment whose translation call fails keeps its original text rather
/// than aborting the whole job (F8's Given/When/Then for a failed segment) --
/// `failed_segments` in the returned outcome lets the UI surface that some
/// cues fell back to the original.
async fn run_translation(
    db: &SharedConn,
    recording_id: &str,
    target_language: &str,
    on_cooldown: &(dyn Fn(u32) + Send + Sync),
    on_progress: &(dyn Fn(f64) + Send + Sync),
    control: &TranslationControl,
) -> Result<TranslationOutcome> {
    let segments = {
        let rid = recording_id.to_string();
        db::with_db(db, move |conn| db::get_transcript_segments(conn, &rid)).await?
    };
    if segments.is_empty() {
        return Err(anyhow::anyhow!("no transcript available to translate"));
    }

    let source_language = {
        let rid = recording_id.to_string();
        db::with_db(db, move |conn| db::get_recording_summary(conn, &rid))
            .await?
            .detected_language
            .unwrap_or_else(|| "the source language".to_string())
    };

    let settings = db::with_db(db, |conn| llm::load_settings(conn)).await?;
    let cloud_token = llm::read_cloud_token()?;

    let total_segments = segments.len();
    let mut translated = Vec::with_capacity(total_segments);
    let mut failed_segments = 0usize;
    for (i, segment) in segments.iter().enumerate() {
        control.wait_if_paused().await;
        if control.is_cancelled() {
            return Ok(TranslationOutcome {
                failed_segments,
                total_segments,
                cancelled: true,
            });
        }
        if settings.provider == llm::ProviderKind::Local {
            crate::thermal::wait_for_safe_temp(on_cooldown).await;
        }
        match llm::translate_segment(
            &settings,
            cloud_token.as_deref(),
            &segment.text,
            &source_language,
            target_language,
        )
        .await
        {
            Ok(text) => translated.push(db::Segment {
                start_ms: segment.start_ms,
                end_ms: segment.end_ms,
                text,
            }),
            Err(e) => {
                tracing::warn!("segment translation failed, keeping original text: {e:#}");
                failed_segments += 1;
                translated.push(segment.clone());
            }
        }
        on_progress((i + 1) as f64 / total_segments as f64);
    }

    {
        let rid = recording_id.to_string();
        let lang = target_language.to_string();
        db::with_db(db, move |conn| {
            db::replace_translated_segments(conn, &rid, &lang, &translated)
        })
        .await?;
    }

    Ok(TranslationOutcome { failed_segments, total_segments, cancelled: false })
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

    /// A no-op `on_cooldown` callback for tests that don't exercise the
    /// thermal-pause path (see `thermal::wait_for_safe_temp`).
    fn no_cooldown(_gpu_temp_c: u32) {}

    /// A no-op `on_progress` callback for tests that don't assert on
    /// translation progress events.
    fn no_progress(_progress: f64) {}

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

        run_summarization(&db, &recording_id, &no_cooldown)
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

    /// F8's Given/When/Then: a ready transcript translated segment-by-segment
    /// via LLM, with each translated segment's timecodes matching the
    /// original.
    #[tokio::test]
    async fn run_translation_persists_translated_segments_with_matching_timecodes() {
        let base_dir = std::env::temp_dir().join("screen_recorder_translate_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();
        let db = db::open(&base_dir.join("db.sqlite3")).unwrap();

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "Bonjour le monde"}}]
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

        let outcome = run_translation(
            &db,
            &recording_id,
            "fr",
            &no_cooldown,
            &no_progress,
            &TranslationControl::default(),
        )
            .await
            .expect("translation should succeed");
        assert_eq!(outcome.failed_segments, 0);
        assert_eq!(outcome.total_segments, 1);

        let translated = {
            let rid = recording_id.clone();
            db::with_db(&db, move |conn| db::get_translated_segments(conn, &rid, "fr"))
                .await
                .unwrap()
        };
        assert_eq!(translated.len(), 1);
        assert_eq!(translated[0].text, "Bonjour le monde");
        assert_eq!(translated[0].start_ms, 0);
        assert_eq!(translated[0].end_ms, 1000);
    }

    /// F8's failure Given/When/Then: a segment whose translation call fails
    /// keeps its original text and the job still completes (rather than
    /// aborting), with the failure counted in the outcome.
    #[tokio::test]
    async fn run_translation_falls_back_to_original_text_on_provider_error() {
        let base_dir = std::env::temp_dir().join("screen_recorder_translate_fail_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();
        let db = db::open(&base_dir.join("db.sqlite3")).unwrap();

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(500))
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

        let outcome = run_translation(
            &db,
            &recording_id,
            "fr",
            &no_cooldown,
            &no_progress,
            &TranslationControl::default(),
        )
            .await
            .expect("job should still complete despite a failed segment");
        assert_eq!(outcome.failed_segments, 1);
        assert_eq!(outcome.total_segments, 1);

        let translated = {
            let rid = recording_id.clone();
            db::with_db(&db, move |conn| db::get_translated_segments(conn, &rid, "fr"))
                .await
                .unwrap()
        };
        assert_eq!(translated[0].text, "hello world", "should fall back to the original text");
    }
}
