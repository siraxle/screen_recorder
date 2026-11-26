//! Keeps a recording's on-disk names in sync with its user-facing title.
//!
//! Each recording lives in its own folder (see `commands::stop_recording` /
//! `commands::import_recording`), e.g. `recording-2026-11-26_10-00-00/recording.mp4`,
//! with derived files next to the video sharing its stem
//! (`recording.web.mp4`, `recording.srt`, ...) plus subtitle exports with
//! their own names (`en.srt`, `dual.ru.vtt`, ...). Renaming a recording
//! renames the folder to the title and every `<stem>.*` file to `<title>.*`,
//! so the recordings folder in Explorer/Finder shows the same name as the app.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Longest folder/file stem we produce. Keeps full paths comfortably below
/// Windows' legacy 260-char MAX_PATH even under a deep recordings folder.
const MAX_NAME_CHARS: usize = 100;

/// Turns a user-typed title into a name valid as a file/folder name on
/// Windows, macOS and Linux. Returns `None` when nothing usable is left
/// (e.g. the title was only `???`).
pub fn sanitize_file_name(title: &str) -> Option<String> {
    let cleaned: String = title
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    // Collapse whitespace runs left behind by replaced characters.
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = collapsed.chars().take(MAX_NAME_CHARS).collect();
    // Windows silently strips trailing dots/spaces, which would make the
    // name we store differ from the one on disk.
    let trimmed = truncated.trim_end_matches(['.', ' ']).trim_start().to_string();
    if trimmed.is_empty() {
        return None;
    }
    let upper = trimmed.to_ascii_uppercase();
    let base = upper.split('.').next().unwrap_or("");
    let reserved = matches!(base, "CON" | "PRN" | "AUX" | "NUL")
        || ((base.starts_with("COM") || base.starts_with("LPT"))
            && base.len() == 4
            && base.as_bytes()[3].is_ascii_digit());
    Some(if reserved { format!("{trimmed}_") } else { trimmed })
}

/// Result of `rename_recording_files`: the new locations of the paths the DB
/// tracks, plus what's needed to undo the rename.
#[derive(Debug)]
pub struct RenamedRecording {
    pub video_path: PathBuf,
    pub compressed_path: Option<PathBuf>,
    undo: Vec<(PathBuf, PathBuf)>,
}

impl RenamedRecording {
    /// Best-effort revert, used when persisting the new paths fails so disk
    /// and DB don't drift apart.
    pub fn rollback(self) {
        for (from, to) in self.undo.into_iter().rev() {
            if let Err(e) = std::fs::rename(&to, &from) {
                tracing::error!("failed to roll back rename {to:?} -> {from:?}: {e}");
            }
        }
    }
}

fn same_name(a: &str, b: &str) -> bool {
    // Case-insensitive filesystems (Windows/macOS default) treat these as the
    // same entry, so a case-only rename must not be seen as a collision.
    a.to_lowercase() == b.to_lowercase()
}

/// Picks `name`, or `name (2)`, `name (3)`, ... -- the first one for which
/// `taken` is false.
fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    (2..)
        .map(|n| format!("{name} ({n})"))
        .find(|candidate| !taken(candidate))
        .expect("unbounded range always yields a free name")
}

/// Renames the recording's folder and the video (plus every sibling sharing
/// its stem) to `new_name` (already sanitized). `flat_root` is the
/// recordings root: a legacy recording sitting directly in it has no folder
/// of its own, so only its files are renamed.
pub fn rename_recording_files(
    video_path: &Path,
    compressed_path: Option<&Path>,
    new_name: &str,
    flat_root: &Path,
) -> Result<RenamedRecording> {
    let old_folder = video_path
        .parent()
        .context("recording video has no parent folder")?
        .to_path_buf();
    let old_stem = video_path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("recording video has no file name")?
        .to_string();
    if !video_path.is_file() {
        bail!("recording file {video_path:?} not found");
    }
    let owns_folder = old_folder != flat_root;

    let mut undo: Vec<(PathBuf, PathBuf)> = Vec::new();
    let fail = |undo: Vec<(PathBuf, PathBuf)>, err: anyhow::Error| -> anyhow::Error {
        RenamedRecording { video_path: PathBuf::new(), compressed_path: None, undo }.rollback();
        err
    };

    // 1. Folder. Done first: on Windows it fails fast if any file inside is
    //    still open (player, HandBrake, ...), before anything has moved.
    let folder = if owns_folder {
        let old_folder_name = old_folder
            .file_name()
            .and_then(|s| s.to_str())
            .context("recording folder has no name")?
            .to_string();
        let parent = old_folder.parent().context("recording folder has no parent")?;
        let folder_name = unique_name(new_name, |candidate| {
            !same_name(candidate, &old_folder_name) && parent.join(candidate).exists()
        });
        let new_folder = parent.join(&folder_name);
        if folder_name != old_folder_name {
            std::fs::rename(&old_folder, &new_folder).with_context(|| {
                format!("could not rename folder {old_folder:?} (is a file in it open in another program?)")
            })?;
            undo.push((old_folder.clone(), new_folder.clone()));
        }
        new_folder
    } else {
        old_folder.clone()
    };

    // 2. Files sharing the video's stem: `<stem>.mp4`, `<stem>.web.mp4`, ...
    let prefix = format!("{old_stem}.");
    let mut siblings: Vec<String> = Vec::new();
    let entries = match std::fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(e) => return Err(fail(undo, anyhow::Error::new(e).context("failed to list recording folder"))),
    };
    let mut all_names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            all_names.push(name.to_string());
            if name.starts_with(&prefix) {
                siblings.push(name.to_string());
            }
        }
    }
    // A stem whose renamed siblings would overwrite an unrelated file
    // (e.g. title "en" turning `recording.srt` into an existing `en.srt`)
    // gets a numeric suffix instead.
    let new_stem = unique_name(new_name, |candidate| {
        siblings.iter().any(|s| {
            let target = format!("{candidate}.{}", &s[prefix.len()..]);
            all_names
                .iter()
                .any(|existing| same_name(existing, &target) && !siblings.contains(existing))
        })
    });

    if new_stem != old_stem {
        for name in &siblings {
            let from = folder.join(name);
            let to = folder.join(format!("{new_stem}.{}", &name[prefix.len()..]));
            if let Err(e) = std::fs::rename(&from, &to) {
                return Err(fail(
                    undo,
                    anyhow::Error::new(e).context(format!(
                        "could not rename {from:?} (is it open in another program?)"
                    )),
                ));
            }
            undo.push((from, to));
        }
    }

    // Map a DB-tracked path into the new layout.
    let relocate = |path: &Path| -> PathBuf {
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            return path.to_path_buf();
        };
        if path.parent() != Some(old_folder.as_path()) {
            return path.to_path_buf(); // lives elsewhere; untouched
        }
        let name = match name.strip_prefix(&prefix) {
            Some(rest) if new_stem != old_stem => format!("{new_stem}.{rest}"),
            _ => name.to_string(),
        };
        folder.join(name)
    };

    Ok(RenamedRecording {
        video_path: relocate(video_path),
        compressed_path: compressed_path.map(relocate),
        undo,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("screen_recorder_rename_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sanitize_strips_invalid_characters_and_reserved_names() {
        assert_eq!(sanitize_file_name("Встреча: план/итоги?").as_deref(), Some("Встреча план итоги"));
        assert_eq!(sanitize_file_name("  demo...  ").as_deref(), Some("demo"));
        assert_eq!(sanitize_file_name("CON").as_deref(), Some("CON_"));
        assert_eq!(sanitize_file_name("com1.txt").as_deref(), Some("com1.txt_"));
        assert_eq!(sanitize_file_name("???"), None);
        assert_eq!(sanitize_file_name(&"a".repeat(300)).unwrap().chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn renames_folder_video_and_stem_siblings() {
        let root = fresh_dir("basic");
        let folder = root.join("recording-2026-11-26_10-00-00");
        std::fs::create_dir_all(&folder).unwrap();
        for f in ["recording.mp4", "recording.web.mp4", "recording.srt", "en.srt"] {
            std::fs::write(folder.join(f), f).unwrap();
        }

        let renamed = rename_recording_files(
            &folder.join("recording.mp4"),
            Some(&folder.join("recording.web.mp4")),
            "Планёрка",
            &root,
        )
        .unwrap();

        let new_folder = root.join("Планёрка");
        assert!(!folder.exists());
        assert_eq!(renamed.video_path, new_folder.join("Планёрка.mp4"));
        assert_eq!(renamed.compressed_path, Some(new_folder.join("Планёрка.web.mp4")));
        for f in ["Планёрка.mp4", "Планёрка.web.mp4", "Планёрка.srt", "en.srt"] {
            assert!(new_folder.join(f).is_file(), "{f} missing");
        }
    }

    #[test]
    fn folder_name_collision_gets_numeric_suffix() {
        let root = fresh_dir("collision");
        std::fs::create_dir_all(root.join("Demo")).unwrap();
        let folder = root.join("recording-1");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("recording.mp4"), "x").unwrap();

        let renamed = rename_recording_files(&folder.join("recording.mp4"), None, "Demo", &root).unwrap();
        assert_eq!(renamed.video_path, root.join("Demo (2)").join("Demo.mp4"));
    }

    #[test]
    fn stem_colliding_with_subtitle_export_gets_numeric_suffix() {
        let root = fresh_dir("stem_collision");
        let folder = root.join("recording-1");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("recording.mp4"), "video").unwrap();
        std::fs::write(folder.join("recording.srt"), "video-srt").unwrap();
        std::fs::write(folder.join("en.srt"), "export").unwrap();

        let renamed = rename_recording_files(&folder.join("recording.mp4"), None, "en", &root).unwrap();
        let new_folder = root.join("en");
        assert_eq!(renamed.video_path, new_folder.join("en (2).mp4"));
        assert_eq!(std::fs::read_to_string(new_folder.join("en.srt")).unwrap(), "export");
        assert_eq!(std::fs::read_to_string(new_folder.join("en (2).srt")).unwrap(), "video-srt");
    }

    #[test]
    fn rename_again_and_rollback_restore_original_layout() {
        let root = fresh_dir("rollback");
        let folder = root.join("recording-1");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("recording.mp4"), "x").unwrap();

        let first = rename_recording_files(&folder.join("recording.mp4"), None, "One", &root).unwrap();
        // Case-only rename of an already renamed recording.
        let second = rename_recording_files(&first.video_path, None, "one", &root).unwrap();
        assert_eq!(second.video_path, root.join("one").join("one.mp4"));
        assert!(second.video_path.is_file());

        second.rollback();
        first.rollback();
        assert!(folder.join("recording.mp4").is_file());
    }

    #[test]
    fn legacy_flat_recording_only_renames_files() {
        let root = fresh_dir("flat");
        std::fs::write(root.join("old.mp4"), "x").unwrap();
        let renamed = rename_recording_files(&root.join("old.mp4"), None, "New", &root).unwrap();
        assert_eq!(renamed.video_path, root.join("New.mp4"));
        assert!(renamed.video_path.is_file());
    }
}
