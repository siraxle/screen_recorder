//! Persists the user's "use this Python/FFmpeg that's already on my PC"
//! choice (from `setup::scan` or a manually browsed-to folder) across
//! restarts. Stored as a small JSON file under `<app_data_dir>/runtime/`
//! rather than the app's SQLite settings table, so `setup` stays
//! self-contained -- everything else in this module is already file-based
//! (see `paths`'s marker files).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{paths, DependencyId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencySource {
    /// The app's own managed install under `runtime/` -- the default, and
    /// the only source `DependencyId::GpuPackages` ever has (see module doc
    /// on `setup::mod`).
    Bundled,
    /// Auto-detected elsewhere on the machine via `setup::scan`.
    System,
    /// A folder the user explicitly browsed to.
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Override {
    pub path: PathBuf,
    pub source: DependencySource,
}

fn overrides_file(app_data_dir: &Path) -> PathBuf {
    paths::runtime_dir(app_data_dir).join("dependency-overrides.json")
}

/// Never errors -- a missing or corrupt overrides file just means "no
/// overrides," same as a fresh install, so `check_all` can call this
/// unconditionally on every poll without a `Result` to unwrap.
pub fn load(app_data_dir: &Path) -> HashMap<DependencyId, Override> {
    let Ok(text) = std::fs::read_to_string(overrides_file(app_data_dir)) else {
        return HashMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save(app_data_dir: &Path, overrides: &HashMap<DependencyId, Override>) -> Result<()> {
    let file = overrides_file(app_data_dir);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(overrides)?;
    std::fs::write(&file, json).with_context(|| format!("failed to write {file:?}"))
}

pub fn set(app_data_dir: &Path, id: DependencyId, ov: Override) -> Result<()> {
    let mut all = load(app_data_dir);
    all.insert(id, ov);
    save(app_data_dir, &all)
}

pub fn clear(app_data_dir: &Path, id: DependencyId) -> Result<()> {
    let mut all = load(app_data_dir);
    all.remove(&id);
    save(app_data_dir, &all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_load_clear_round_trip() {
        let dir = std::env::temp_dir().join(format!("setup-overrides-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        assert!(load(&dir).is_empty());

        set(&dir, DependencyId::Ffmpeg, Override { path: PathBuf::from("C:\\ffmpeg\\bin"), source: DependencySource::Custom })
            .unwrap();
        let loaded = load(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[&DependencyId::Ffmpeg].path, PathBuf::from("C:\\ffmpeg\\bin"));
        assert_eq!(loaded[&DependencyId::Ffmpeg].source, DependencySource::Custom);

        clear(&dir, DependencyId::Ffmpeg).unwrap();
        assert!(load(&dir).is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_tolerates_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("setup-overrides-test-missing-{}", uuid::Uuid::new_v4()));
        assert!(load(&dir).is_empty());
    }
}
