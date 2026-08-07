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
    if version < 5 {
        conn.execute_batch(
            "
            ALTER TABLE recordings ADD COLUMN duration_ms INTEGER;
            PRAGMA user_version = 5;
            ",
        )
        .context("failed to apply migration to user_version 5")?;
    }
    if version < 6 {
        // F8/F9: translated subtitles. `subtitle_language_override` is the
        // target language for the next `translate` job (mirrors
        // `summary_language_override`'s "set immediately before enqueueing"
        // convention). `translated_segments` is keyed by (recording_id,
        // language) rather than just recording_id so a prior translation
        // isn't lost if the user picks a different subtitle language later.
        conn.execute_batch(
            "
            ALTER TABLE recordings ADD COLUMN subtitle_language_override TEXT;
            CREATE TABLE translated_segments (
                recording_id TEXT NOT NULL REFERENCES recordings(id),
                language TEXT NOT NULL,
                segment_index INTEGER NOT NULL,
                start_ms INTEGER NOT NULL,
                end_ms INTEGER NOT NULL,
                text TEXT NOT NULL,
                PRIMARY KEY (recording_id, language, segment_index)
            );
            PRAGMA user_version = 6;
            ",
        )
        .context("failed to apply migration to user_version 6")?;
    }
    if version < 7 {
        // F11: full-text search across transcripts and summaries. Both FTS5
        // tables are "external content" (they store no text of their own,
        // just the index) pointing at `transcript_segments`/`summaries`,
        // which are ordinary rowid tables (a declared composite/TEXT PRIMARY
        // KEY doesn't turn a table into WITHOUT ROWID) so `content_rowid`
        // can be the implicit `rowid`. Triggers keep the index in sync with
        // every insert/update/delete; the bundled SQLite build always
        // compiles in FTS5 (see libsqlite3-sys build.rs), so no extra Cargo
        // feature is needed. Upgrading DBs already have rows in the content
        // tables, so this migration backfills both indexes once.
        conn.execute_batch(
            "
            CREATE VIRTUAL TABLE transcript_fts USING fts5(
                text,
                content='transcript_segments',
                content_rowid='rowid'
            );
            CREATE TRIGGER transcript_segments_ai AFTER INSERT ON transcript_segments BEGIN
                INSERT INTO transcript_fts(rowid, text) VALUES (new.rowid, new.text);
            END;
            CREATE TRIGGER transcript_segments_ad AFTER DELETE ON transcript_segments BEGIN
                INSERT INTO transcript_fts(transcript_fts, rowid, text) VALUES('delete', old.rowid, old.text);
            END;
            CREATE TRIGGER transcript_segments_au AFTER UPDATE ON transcript_segments BEGIN
                INSERT INTO transcript_fts(transcript_fts, rowid, text) VALUES('delete', old.rowid, old.text);
                INSERT INTO transcript_fts(rowid, text) VALUES (new.rowid, new.text);
            END;
            INSERT INTO transcript_fts(rowid, text) SELECT rowid, text FROM transcript_segments;

            CREATE VIRTUAL TABLE summary_fts USING fts5(
                overview,
                key_points,
                content='summaries',
                content_rowid='rowid'
            );
            CREATE TRIGGER summaries_ai AFTER INSERT ON summaries BEGIN
                INSERT INTO summary_fts(rowid, overview, key_points) VALUES (new.rowid, new.overview, new.key_points);
            END;
            CREATE TRIGGER summaries_ad AFTER DELETE ON summaries BEGIN
                INSERT INTO summary_fts(summary_fts, rowid, overview, key_points) VALUES('delete', old.rowid, old.overview, old.key_points);
            END;
            CREATE TRIGGER summaries_au AFTER UPDATE ON summaries BEGIN
                INSERT INTO summary_fts(summary_fts, rowid, overview, key_points) VALUES('delete', old.rowid, old.overview, old.key_points);
                INSERT INTO summary_fts(rowid, overview, key_points) VALUES (new.rowid, new.overview, new.key_points);
            END;
            INSERT INTO summary_fts(rowid, overview, key_points) SELECT rowid, overview, key_points FROM summaries;

            PRAGMA user_version = 7;
            ",
        )
        .context("failed to apply migration to user_version 7")?;
    }
    if version < 8 {
        // User-editable display name, independent of `created_at` (the
        // default shown in the UI when this is unset).
        conn.execute_batch(
            "
            ALTER TABLE recordings ADD COLUMN title TEXT;
            PRAGMA user_version = 8;
            ",
        )
        .context("failed to apply migration to user_version 8")?;
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
    pub duration_ms: Option<i64>,
    pub title: Option<String>,
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
    duration_ms: Option<i64>,
) -> Result<String> {
    let created_at = now();
    conn.execute(
        "INSERT INTO recordings (id, video_path, created_at, status, language_override, detected_language, duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
        rusqlite::params![
            recording_id,
            video_path.to_string_lossy(),
            created_at,
            PipelineStatus::Recorded.as_str(),
            language_override,
            duration_ms,
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

/// Inserts a `translate` job for a recording whose transcript is already
/// ready (F8: like `summarize`, user-triggered and layered on top of F2).
/// Deliberately independent of any `summarize` job for the same recording —
/// per CLAUDE.md, `translating` must be able to run concurrently with
/// `summarizing` rather than blocking it.
pub fn insert_translate_job(conn: &Connection, recording_id: &str) -> Result<String> {
    let job_id = uuid::Uuid::new_v4().to_string();
    let created_at = now();
    conn.execute(
        "INSERT INTO jobs (id, recording_id, stage, status, error, created_at, updated_at)
         VALUES (?1, ?2, 'translate', ?3, NULL, ?4, ?4)",
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

/// `None` means "subtitles use the video language, no translation" (F8's
/// default). Set immediately before enqueueing a `translate` job, mirroring
/// `set_recording_summary_language_override`.
pub fn set_recording_subtitle_language_override(
    conn: &Connection,
    recording_id: &str,
    language: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE recordings SET subtitle_language_override = ?1 WHERE id = ?2",
        rusqlite::params![language, recording_id],
    )?;
    Ok(())
}

pub fn get_recording_subtitle_language_override(
    conn: &Connection,
    recording_id: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT subtitle_language_override FROM recordings WHERE id = ?1",
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
        "SELECT id, video_path, created_at, status, detected_language, duration_ms, title FROM recordings WHERE id = ?1",
        [recording_id],
        |row| {
            let status: String = row.get(3)?;
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, status, row.get::<_, Option<String>>(4)?, row.get::<_, Option<i64>>(5)?, row.get::<_, Option<String>>(6)?))
        },
    )
    .with_context(|| format!("recording {recording_id} not found"))
    .and_then(|(id, video_path, created_at, status, detected_language, duration_ms, title)| {
        Ok(RecordingSummary {
            id,
            video_path,
            created_at,
            status: PipelineStatus::parse(&status)?,
            detected_language,
            duration_ms,
            title,
        })
    })
}

pub fn list_recordings(conn: &Connection) -> Result<Vec<RecordingSummary>> {
    let mut stmt = conn.prepare(
        "SELECT id, video_path, created_at, status, detected_language, duration_ms, title
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
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    rows.into_iter()
        .map(|(id, video_path, created_at, status, detected_language, duration_ms, title)| {
            Ok(RecordingSummary {
                id,
                video_path,
                created_at,
                status: PipelineStatus::parse(&status)?,
                detected_language,
                duration_ms,
                title,
            })
        })
        .collect()
}

/// `None`/empty clears the custom title, falling back to the UI's default
/// display (formatted `created_at`).
pub fn set_recording_title(
    conn: &Connection,
    recording_id: &str,
    title: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE recordings SET title = ?1 WHERE id = ?2",
        rusqlite::params![title, recording_id],
    )?;
    Ok(())
}

/// Deletes a recording and everything derived from it (transcript,
/// translations, summary, jobs). Returns the recording's `video_path` so the
/// caller can also remove the on-disk recording folder — each recording
/// lives in its own folder (see `commands::stop_recording`), so that path's
/// parent is safe to remove entirely.
pub fn delete_recording(conn: &Connection, recording_id: &str) -> Result<PathBuf> {
    let video_path = get_recording_video_path(conn, recording_id)?;
    conn.execute(
        "DELETE FROM translated_segments WHERE recording_id = ?1",
        [recording_id],
    )?;
    conn.execute(
        "DELETE FROM transcript_segments WHERE recording_id = ?1",
        [recording_id],
    )?;
    conn.execute("DELETE FROM summaries WHERE recording_id = ?1", [recording_id])?;
    conn.execute("DELETE FROM jobs WHERE recording_id = ?1", [recording_id])?;
    conn.execute("DELETE FROM recordings WHERE id = ?1", [recording_id])?;
    Ok(video_path)
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

/// Replaces the cached translation for `(recording_id, language)` (F8: a
/// segment-by-segment translation, so a failed segment's original text is
/// already baked into `segments` by the caller — see `pipeline::run_translation`).
/// Keyed by language rather than a single slot per recording so re-running
/// `list_recordings`/export after switching subtitle language doesn't lose
/// an earlier translation.
pub fn replace_translated_segments(
    conn: &Connection,
    recording_id: &str,
    language: &str,
    segments: &[Segment],
) -> Result<()> {
    conn.execute(
        "DELETE FROM translated_segments WHERE recording_id = ?1 AND language = ?2",
        rusqlite::params![recording_id, language],
    )?;
    for (index, segment) in segments.iter().enumerate() {
        conn.execute(
            "INSERT INTO translated_segments (recording_id, language, segment_index, start_ms, end_ms, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                recording_id,
                language,
                index as i64,
                segment.start_ms,
                segment.end_ms,
                segment.text,
            ],
        )?;
    }
    Ok(())
}

/// Empty when no translation has been generated yet for this language (the
/// caller — `commands::export_subtitles` — treats that as "run Translate
/// first" rather than exporting an empty file).
pub fn get_translated_segments(
    conn: &Connection,
    recording_id: &str,
    language: &str,
) -> Result<Vec<Segment>> {
    let mut stmt = conn.prepare(
        "SELECT start_ms, end_ms, text FROM translated_segments
         WHERE recording_id = ?1 AND language = ?2 ORDER BY segment_index ASC",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![recording_id, language], |row| {
            Ok(Segment {
                start_ms: row.get(0)?,
                end_ms: row.get(1)?,
                text: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Every language with a persisted translation for this recording -- lets
/// the UI rehydrate `translatedLanguages` (and so the export/"Save" button)
/// after a reload, instead of relying solely on the in-session
/// `translation-updated` event that produced it originally.
pub fn list_translated_languages(conn: &Connection, recording_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT language FROM translated_segments WHERE recording_id = ?1",
    )?;
    let rows = stmt
        .query_map([recording_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One matching passage within a recording (F11). `snippet` wraps each
/// matched term in `\u{1}...\u{2}` markers rather than HTML so the frontend
/// can render highlights without `dangerouslySetInnerHTML` on
/// transcript/summary text the app doesn't control.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    pub source: String, // "transcript" | "summary"
    pub snippet: String,
    pub start_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub recording: RecordingSummary,
    pub matches: Vec<SearchMatch>,
}

const SNIPPET_MARK_START: &str = "\u{1}";
const SNIPPET_MARK_END: &str = "\u{2}";

/// Turns free-text user input into an FTS5 `MATCH` expression. Each token is
/// quoted (with embedded quotes doubled per FTS5's escaping rule) and given
/// a trailing `*` for prefix matching, then tokens are joined with FTS5's
/// implicit `AND` -- this way punctuation/operators the user types (`-`,
/// `"`, `AND`, `:`) are always treated as literal text instead of tripping
/// FTS5 query syntax errors.
fn build_match_query(query: &str) -> Option<String> {
    let parts: Vec<String> = query
        .split_whitespace()
        .map(|token| format!("\"{}\"*", token.replace('"', "\"\"")))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// F11: full-text search across transcript segments and summaries. Results
/// are grouped by recording (at most 3 transcript matches + 1 summary match
/// each) and ordered most-recent-first, matching `list_recordings` -- bm25
/// scores from the two separate FTS5 tables aren't on a comparable scale, so
/// relevance-ranking across sources isn't attempted.
pub fn search_recordings(conn: &Connection, query: &str) -> Result<Vec<SearchResult>> {
    let Some(match_expr) = build_match_query(query) else {
        return Ok(Vec::new());
    };

    let mut order: Vec<String> = Vec::new();
    let mut matches: std::collections::HashMap<String, Vec<SearchMatch>> =
        std::collections::HashMap::new();
    let mut push_match = |recording_id: String, m: SearchMatch, per_recording_cap: usize| {
        let entry = matches.entry(recording_id.clone()).or_insert_with(|| {
            order.push(recording_id.clone());
            Vec::new()
        });
        if entry.iter().filter(|existing| existing.source == m.source).count() < per_recording_cap {
            entry.push(m);
        }
    };

    let mut transcript_stmt = conn.prepare(
        "SELECT ts.recording_id, ts.start_ms,
                snippet(transcript_fts, 0, ?2, ?3, '…', 12)
         FROM transcript_fts
         JOIN transcript_segments ts ON ts.rowid = transcript_fts.rowid
         WHERE transcript_fts MATCH ?1
         ORDER BY rank
         LIMIT 200",
    )?;
    let transcript_rows = transcript_stmt
        .query_map(
            rusqlite::params![match_expr, SNIPPET_MARK_START, SNIPPET_MARK_END],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (recording_id, start_ms, snippet) in transcript_rows {
        push_match(
            recording_id,
            SearchMatch { source: "transcript".to_string(), snippet, start_ms: Some(start_ms) },
            3,
        );
    }

    let mut summary_stmt = conn.prepare(
        "SELECT s.recording_id,
                snippet(summary_fts, 0, ?2, ?3, '…', 12),
                snippet(summary_fts, 1, ?2, ?3, '…', 12)
         FROM summary_fts
         JOIN summaries s ON s.rowid = summary_fts.rowid
         WHERE summary_fts MATCH ?1
         ORDER BY rank
         LIMIT 100",
    )?;
    let summary_rows = summary_stmt
        .query_map(
            rusqlite::params![match_expr, SNIPPET_MARK_START, SNIPPET_MARK_END],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (recording_id, overview_snippet, key_points_snippet) in summary_rows {
        let snippet = if overview_snippet.contains(SNIPPET_MARK_START) {
            overview_snippet
        } else {
            key_points_snippet
        };
        push_match(
            recording_id,
            SearchMatch { source: "summary".to_string(), snippet, start_ms: None },
            1,
        );
    }

    let mut results: Vec<SearchResult> = order
        .into_iter()
        .filter_map(|recording_id| {
            let recording = get_recording_summary(conn, &recording_id).ok()?;
            let matches = matches.remove(&recording_id)?;
            Some(SearchResult { recording, matches })
        })
        .collect();
    results.sort_by(|a, b| b.recording.created_at.cmp(&a.recording.created_at));
    Ok(results)
}

/// True if a `translate` job for this recording is already queued or
/// running -- guards `commands::translate_subtitles` against enqueueing a
/// second job that would silently redo the same LLM work `run_translation`
/// already has in flight (it has no partial-progress resume, only a
/// full-segments rewrite at the end).
pub fn has_active_translate_job(conn: &Connection, recording_id: &str) -> Result<bool> {
    let found = conn
        .query_row(
            "SELECT 1 FROM jobs WHERE recording_id = ?1 AND stage = 'translate' \
             AND status IN ('pending', 'running') LIMIT 1",
            [recording_id],
            |_| Ok(()),
        )
        .optional()?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    /// F11: a transcript match reports its timecode and highlights the
    /// matched term; a summary match reports no timecode.
    #[test]
    fn search_finds_transcript_and_summary_matches_with_highlighting() {
        let conn = open_test_db();
        insert_recording_with_transcribe_job(&conn, "rec-1", Path::new("rec-1.mp4"), None, None)
            .unwrap();
        replace_transcript_segments(
            &conn,
            "rec-1",
            &[Segment { start_ms: 1000, end_ms: 2000, text: "let's discuss the budget".into() }],
        )
        .unwrap();
        replace_summary(
            &conn,
            "rec-1",
            &Summary {
                overview: "Team agreed on the roadmap.".into(),
                key_points: vec!["Ship by Friday".into()],
                language: "en".into(),
            },
        )
        .unwrap();

        let budget_hits = search_recordings(&conn, "budget").unwrap();
        assert_eq!(budget_hits.len(), 1);
        assert_eq!(budget_hits[0].matches.len(), 1);
        let m = &budget_hits[0].matches[0];
        assert_eq!(m.source, "transcript");
        assert_eq!(m.start_ms, Some(1000));
        assert!(m.snippet.contains(&format!("{SNIPPET_MARK_START}budget{SNIPPET_MARK_END}")));

        let roadmap_hits = search_recordings(&conn, "roadmap").unwrap();
        assert_eq!(roadmap_hits.len(), 1);
        assert_eq!(roadmap_hits[0].matches[0].source, "summary");
        assert_eq!(roadmap_hits[0].matches[0].start_ms, None);

        assert!(search_recordings(&conn, "nonexistentword").unwrap().is_empty());
    }

    /// Punctuation/operators a user might type (quotes, FTS5 keywords) must
    /// not crash `MATCH` -- `build_match_query` quotes every token.
    #[test]
    fn search_handles_special_characters_without_error() {
        let conn = open_test_db();
        insert_recording_with_transcribe_job(&conn, "rec-1", Path::new("rec-1.mp4"), None, None)
            .unwrap();
        replace_transcript_segments(
            &conn,
            "rec-1",
            &[Segment { start_ms: 0, end_ms: 1000, text: "hello world".into() }],
        )
        .unwrap();

        assert!(search_recordings(&conn, "OR \"quoted\" -term:foo").is_ok());
        assert!(search_recordings(&conn, "").unwrap().is_empty());
    }

    /// Prefix matching: a partial word (as typed by the debounced search
    /// box mid-keystroke) should still match.
    #[test]
    fn search_matches_on_word_prefix() {
        let conn = open_test_db();
        insert_recording_with_transcribe_job(&conn, "rec-1", Path::new("rec-1.mp4"), None, None)
            .unwrap();
        replace_transcript_segments(
            &conn,
            "rec-1",
            &[Segment { start_ms: 0, end_ms: 1000, text: "quarterly budgeting review".into() }],
        )
        .unwrap();

        assert_eq!(search_recordings(&conn, "budg").unwrap().len(), 1);
    }
}
