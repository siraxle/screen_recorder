//! SQLite persistence for the transcription job queue (F2).
//!
//! Schema is versioned via `PRAGMA user_version` so future stages
//! (translate/summarize/index, see CLAUDE.md's job queue convention) can add
//! tables/columns without a migration framework. `recordings.status` is a
//! denormalized read for the UI; `jobs` (keyed by `stage`) is the source of
//! truth the queue and crash recovery drive off, since a later stage
//! (`translating`) must be able to run concurrently with another
//! (`summarizing`) — something a single status column on `recordings` can't
//! represent.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub type SharedConn = Arc<Mutex<Connection>>;

pub fn open(path: &Path) -> Result<SharedConn> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open sqlite db at {path:?}"))?;
    run_migrations(&conn)?;
    Ok(Arc::new(Mutex::new(conn)))
}

fn run_migrations(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 1 {
        conn.execute_batch(
            "
            CREATE TABLE recordings (
                id TEXT PRIMARY KEY,
                video_path TEXT NOT NULL,
                created_at TEXT NOT NULL,
                status TEXT NOT NULL,
                language_override TEXT,
                detected_language TEXT
            );
            CREATE TABLE jobs (
                id TEXT PRIMARY KEY,
                recording_id TEXT NOT NULL REFERENCES recordings(id),
                stage TEXT NOT NULL,
                status TEXT NOT NULL,
                error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE transcript_segments (
                recording_id TEXT NOT NULL REFERENCES recordings(id),
                segment_index INTEGER NOT NULL,
                start_ms INTEGER NOT NULL,
                end_ms INTEGER NOT NULL,
                text TEXT NOT NULL,
                PRIMARY KEY (recording_id, segment_index)
            );
            PRAGMA user_version = 1;
            ",
        )
        .context("failed to apply migration to user_version 1")?;
    }
    if version < 2 {
        conn.execute_batch(
            "
            CREATE TABLE settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            PRAGMA user_version = 2;
            ",
        )
        .context("failed to apply migration to user_version 2")?;
    }
    if version < 3 {
        conn.execute_batch(
            "
            CREATE TABLE summaries (
                recording_id TEXT PRIMARY KEY REFERENCES recordings(id),
                overview TEXT NOT NULL,
                key_points TEXT NOT NULL,
                language TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            ALTER TABLE recordings ADD COLUMN summary_language_override TEXT;
            PRAGMA user_version = 3;
            ",
        )
        .context("failed to apply migration to user_version 3")?;
    }
    if version < 4 {
        // Belt-and-suspenders re-check: some pre-release DBs reached
        // user_version 3 before `summary_language_override` was added to
        // that migration block, so this column may still be missing.
        let has_column: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('recordings') WHERE name = 'summary_language_override'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .context("failed to inspect recordings columns")?
            > 0;
        if !has_column {
            conn.execute_batch("ALTER TABLE recordings ADD COLUMN summary_language_override TEXT;")
                .context("failed to add summary_language_override column")?;
        }
        conn.execute_batch("PRAGMA user_version = 4;")
            .context("failed to apply migration to user_version 4")?;
    }
    Ok(())
}

/// Key-value store for user preferences (e.g. the chosen recordings folder,
/// see F1 "let the user choose where recordings are saved"). Deliberately
/// generic rather than a dedicated `recordings_dir` column, since this is
/// the natural place for future single-value settings too.
pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .optional()
    .map_err(Into::into)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// Runs a blocking rusqlite closure off the async runtime's worker pool.
/// Every DB access in this app goes through here rather than an async
/// driver: this is a small local-only app with a single writer, so a
/// pooled/async SQLite layer would add complexity without buying anything.
pub async fn with_db<F, T>(db: &SharedConn, f: F) -> Result<T>
where
    F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let db = Arc::clone(db);
    tokio::task::spawn_blocking(move || {
        let conn = db.lock().map_err(|_| anyhow!("db mutex poisoned"))?;
        f(&conn)
    })
    .await
    .context("db task panicked")?
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStatus {
    Recorded,
    Transcribing,
    Transcribed,
    Summarizing,
    Summarized,
    Failed,
}

impl PipelineStatus {
    fn as_str(self) -> &'static str {
        match self {
            PipelineStatus::Recorded => "recorded",
            PipelineStatus::Transcribing => "transcribing",
            PipelineStatus::Transcribed => "transcribed",
            PipelineStatus::Summarizing => "summarizing",
            PipelineStatus::Summarized => "summarized",
            PipelineStatus::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "recorded" => PipelineStatus::Recorded,
            "transcribing" => PipelineStatus::Transcribing,
            "transcribed" => PipelineStatus::Transcribed,
            "summarizing" => PipelineStatus::Summarizing,
            "summarized" => PipelineStatus::Summarized,
            "failed" => PipelineStatus::Failed,
            other => return Err(anyhow!("unknown recording status {other:?}")),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
        }
    }
}

/// One transcript utterance, shared between the STT worker's parsed output,
/// DB storage, and subtitle formatting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingSummary {
    pub id: String,
    pub video_path: String,
    pub created_at: String,
    pub status: PipelineStatus,
    pub detected_language: Option<String>,
}

pub struct JobRow {
    pub id: String,
    pub recording_id: String,
    pub stage: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub overview: String,
    pub key_points: Vec<String>,
    pub language: String,
}

fn now() -> String {
    chrono::Local::now().to_rfc3339()
}

/// Inserts the `recordings` row and its `transcribe` job in one go — a
/// recording is never persisted without a job to process it.
pub fn insert_recording_with_transcribe_job(
    conn: &Connection,
    recording_id: &str,
    video_path: &Path,
    language_override: Option<&str>,
) -> Result<String> {
    let created_at = now();
    conn.execute(
        "INSERT INTO recordings (id, video_path, created_at, status, language_override, detected_language)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
        rusqlite::params![
            recording_id,
            video_path.to_string_lossy(),
            created_at,
            PipelineStatus::Recorded.as_str(),
            language_override,
        ],
    )?;

    let job_id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO jobs (id, recording_id, stage, status, error, created_at, updated_at)
         VALUES (?1, ?2, 'transcribe', ?3, NULL, ?4, ?4)",
        rusqlite::params![job_id, recording_id, JobStatus::Pending.as_str(), created_at],
    )?;
    Ok(job_id)
}

/// Inserts a `summarize` job for a recording whose transcript is already
/// ready (F3: summary generation is user-triggered, unlike `transcribe`
/// which is enqueued automatically on `stop_recording`).
pub fn insert_summarize_job(conn: &Connection, recording_id: &str) -> Result<String> {
    let job_id = uuid::Uuid::new_v4().to_string();
    let created_at = now();
    conn.execute(
        "INSERT INTO jobs (id, recording_id, stage, status, error, created_at, updated_at)
         VALUES (?1, ?2, 'summarize', ?3, NULL, ?4, ?4)",
        rusqlite::params![job_id, recording_id, JobStatus::Pending.as_str(), created_at],
    )?;
    Ok(job_id)
}

/// Resets any job left `running` by a previous crashed run back to
/// `pending` (transcription is all-or-nothing, so re-running from `pending`
/// is the correct resume point), then returns every pending job id so the
/// caller can re-enqueue them.
pub fn recover_pending_job_ids(conn: &Connection) -> Result<Vec<String>> {
    conn.execute(
        "UPDATE jobs SET status = ?1, updated_at = ?2 WHERE status = ?3",
        rusqlite::params![JobStatus::Pending.as_str(), now(), JobStatus::Running.as_str()],
    )?;
    let mut stmt = conn.prepare("SELECT id FROM jobs WHERE status = ?1")?;
    let ids = stmt
        .query_map([JobStatus::Pending.as_str()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

pub fn get_job(conn: &Connection, job_id: &str) -> Result<JobRow> {
    conn.query_row(
        "SELECT id, recording_id, stage, status FROM jobs WHERE id = ?1",
        [job_id],
        |row| {
            Ok(JobRow {
                id: row.get(0)?,
                recording_id: row.get(1)?,
                stage: row.get(2)?,
                status: row.get(3)?,
            })
        },
    )
    .with_context(|| format!("job {job_id} not found"))
}

pub fn set_job_running(conn: &Connection, job_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE jobs SET status = ?1, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![JobStatus::Running.as_str(), now(), job_id],
    )?;
    Ok(())
}

pub fn set_job_done(conn: &Connection, job_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE jobs SET status = ?1, error = NULL, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![JobStatus::Done.as_str(), now(), job_id],
    )?;
    Ok(())
}

pub fn set_job_failed(conn: &Connection, job_id: &str, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE jobs SET status = ?1, error = ?2, updated_at = ?3 WHERE id = ?4",
        rusqlite::params![JobStatus::Failed.as_str(), error, now(), job_id],
    )?;
    Ok(())
}

pub fn set_recording_status(
    conn: &Connection,
    recording_id: &str,
    status: PipelineStatus,
) -> Result<()> {
    conn.execute(
        "UPDATE recordings SET status = ?1 WHERE id = ?2",
        rusqlite::params![status.as_str(), recording_id],
    )?;
    Ok(())
}

pub fn set_recording_detected_language(
    conn: &Connection,
    recording_id: &str,
    language: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE recordings SET detected_language = ?1 WHERE id = ?2",
        rusqlite::params![language, recording_id],
    )?;
    Ok(())
}

pub fn get_recording_language_override(
    conn: &Connection,
    recording_id: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT language_override FROM recordings WHERE id = ?1",
        [recording_id],
        |row| row.get(0),
    )
    .with_context(|| format!("recording {recording_id} not found"))
}

/// `None` means "use the transcript's detected language" (F3's default —
/// summary language independent of, but defaulting to, the transcript
/// language). Set immediately before enqueueing a `summarize` job, mirroring
/// how `language_override` is set once at `insert_recording_with_transcribe_job`.
pub fn set_recording_summary_language_override(
    conn: &Connection,
    recording_id: &str,
    language: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE recordings SET summary_language_override = ?1 WHERE id = ?2",
        rusqlite::params![language, recording_id],
    )?;
    Ok(())
}

pub fn get_recording_summary_language_override(
    conn: &Connection,
    recording_id: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT summary_language_override FROM recordings WHERE id = ?1",
        [recording_id],
        |row| row.get(0),
    )
    .with_context(|| format!("recording {recording_id} not found"))
}

pub fn get_recording_video_path(conn: &Connection, recording_id: &str) -> Result<PathBuf> {
    let path: String = conn
        .query_row(
            "SELECT video_path FROM recordings WHERE id = ?1",
            [recording_id],
            |row| row.get(0),
        )
        .with_context(|| format!("recording {recording_id} not found"))?;
    Ok(PathBuf::from(path))
}

pub fn get_recording_summary(conn: &Connection, recording_id: &str) -> Result<RecordingSummary> {
    conn.query_row(
        "SELECT id, video_path, created_at, status, detected_language FROM recordings WHERE id = ?1",
        [recording_id],
        |row| {
            let status: String = row.get(3)?;
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, status, row.get::<_, Option<String>>(4)?))
        },
    )
    .with_context(|| format!("recording {recording_id} not found"))
    .and_then(|(id, video_path, created_at, status, detected_language)| {
        Ok(RecordingSummary {
            id,
            video_path,
            created_at,
            status: PipelineStatus::parse(&status)?,
            detected_language,
        })
    })
}

pub fn list_recordings(conn: &Connection) -> Result<Vec<RecordingSummary>> {
    let mut stmt = conn.prepare(
        "SELECT id, video_path, created_at, status, detected_language
         FROM recordings ORDER BY created_at DESC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    rows.into_iter()
        .map(|(id, video_path, created_at, status, detected_language)| {
            Ok(RecordingSummary {
                id,
                video_path,
                created_at,
                status: PipelineStatus::parse(&status)?,
                detected_language,
            })
        })
        .collect()
}

pub fn replace_transcript_segments(
    conn: &Connection,
    recording_id: &str,
    segments: &[Segment],
) -> Result<()> {
    conn.execute(
        "DELETE FROM transcript_segments WHERE recording_id = ?1",
        [recording_id],
    )?;
    for (index, segment) in segments.iter().enumerate() {
        conn.execute(
            "INSERT INTO transcript_segments (recording_id, segment_index, start_ms, end_ms, text)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![recording_id, index as i64, segment.start_ms, segment.end_ms, segment.text],
        )?;
    }
    Ok(())
}

pub fn get_transcript_segments(conn: &Connection, recording_id: &str) -> Result<Vec<Segment>> {
    let mut stmt = conn.prepare(
        "SELECT start_ms, end_ms, text FROM transcript_segments
         WHERE recording_id = ?1 ORDER BY segment_index ASC",
    )?;
    let rows = stmt
        .query_map([recording_id], |row| {
            Ok(Segment {
                start_ms: row.get(0)?,
                end_ms: row.get(1)?,
                text: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Upserts the summary for a recording. Summaries can be regenerated (e.g.
/// with a different summary language), so this replaces rather than
/// appends, matching `replace_transcript_segments`'s convention.
pub fn replace_summary(
    conn: &Connection,
    recording_id: &str,
    summary: &Summary,
) -> Result<()> {
    let key_points_json = serde_json::to_string(&summary.key_points)?;
    conn.execute(
        "INSERT INTO summaries (recording_id, overview, key_points, language, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(recording_id) DO UPDATE SET
            overview = excluded.overview,
            key_points = excluded.key_points,
            language = excluded.language,
            created_at = excluded.created_at",
        rusqlite::params![
            recording_id,
            summary.overview,
            key_points_json,
            summary.language,
            now(),
        ],
    )?;
    Ok(())
}

pub fn get_summary(conn: &Connection, recording_id: &str) -> Result<Option<Summary>> {
    conn.query_row(
        "SELECT overview, key_points, language FROM summaries WHERE recording_id = ?1",
        [recording_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )
    .optional()?
    .map(|(overview, key_points_json, language)| {
        let key_points: Vec<String> = serde_json::from_str(&key_points_json)
            .context("stored key_points is not valid JSON")?;
        Ok(Summary { overview, key_points, language })
    })
    .transpose()
}
