//! Archive extraction for the two formats the pinned dependency sources use
//! (python-build-standalone ships `.tar.gz`, BtbN's ffmpeg build ships
//! `.zip`), with per-entry progress reporting.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// A `Read` wrapper that reports cumulative bytes read as a fraction of the
/// file's on-disk (compressed) size. Used to approximate `.tar.gz` extract
/// progress: tar has no cheap way to know the entry count or uncompressed
/// size ahead of time without a second full pass, but bytes consumed from
/// the compressed stream still increases monotonically with entries
/// extracted, which is close enough for a progress bar.
struct ProgressReader<'a, R> {
    inner: R,
    read: u64,
    total: u64,
    on_progress: &'a mut dyn FnMut(f64),
}

impl<R: Read> Read for ProgressReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        if self.total > 0 {
            (self.on_progress)((self.read as f64 / self.total as f64).min(1.0));
        }
        Ok(n)
    }
}

/// Extracts `archive_path` (gzip-compressed tar) into `dest_dir`, which must
/// not already exist (callers extract into a fresh tmp dir, per `setup`'s
/// atomic-install convention).
pub fn extract_tar_gz(archive_path: &Path, dest_dir: &Path, mut on_progress: impl FnMut(f64)) -> Result<()> {
    let file = File::open(archive_path).with_context(|| format!("failed to open {archive_path:?}"))?;
    let total = file.metadata().map(|m| m.len()).unwrap_or(0);
    std::fs::create_dir_all(dest_dir)?;

    let progress_reader = ProgressReader { inner: file, read: 0, total, on_progress: &mut on_progress };
    let decoder = flate2::read::GzDecoder::new(progress_reader);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(dest_dir)
        .with_context(|| format!("failed to extract {archive_path:?} into {dest_dir:?}"))?;
    Ok(())
}

/// Extracts `archive_path` (a zip) into `dest_dir`, entry by entry so
/// progress is `entries_done / total_entries` rather than a single jump.
pub fn extract_zip(archive_path: &Path, dest_dir: &Path, mut on_progress: impl FnMut(f64)) -> Result<()> {
    let file = File::open(archive_path).with_context(|| format!("failed to open {archive_path:?}"))?;
    let mut archive = zip::ZipArchive::new(file).with_context(|| format!("failed to read {archive_path:?} as zip"))?;
    std::fs::create_dir_all(dest_dir)?;

    let total = archive.len().max(1);
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(relative) = entry.enclosed_name() else {
            continue; // skip entries with unsafe/absolute paths rather than erroring the whole install
        };
        let out_path = dest_dir.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out_file = File::create(&out_path)
                .with_context(|| format!("failed to create {out_path:?}"))?;
            std::io::copy(&mut entry, &mut out_file)?;
        }
        on_progress(((i + 1) as f64 / total as f64).min(1.0));
    }
    Ok(())
}

/// Returns the sole directory entry directly under `dir` -- both pinned
/// archives (python-build-standalone's `python/`, BtbN's
/// `ffmpeg-<hash>-win64-gpl-<ver>/`) extract to exactly one top-level
/// directory whose exact name either isn't canonical (python) or changes
/// every release (ffmpeg's git-hash suffix), so callers rename this into a
/// fixed final name instead of hardcoding it.
pub fn single_subdir(dir: &Path) -> Result<PathBuf> {
    let mut dirs = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read {dir:?}"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir());
    let first = dirs.next().ok_or_else(|| anyhow!("no subdirectory found under {dir:?}"))?;
    if dirs.next().is_some() {
        return Err(anyhow!("expected exactly one subdirectory under {dir:?}, found more than one"));
    }
    Ok(first.path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("archive-test-{label}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn extract_zip_reproduces_file_contents() {
        let root = scratch_dir("zip");
        std::fs::create_dir_all(&root).unwrap();
        let archive_path = root.join("fixture.zip");
        {
            let file = File::create(&archive_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            writer.start_file("topdir/bin/hello.txt", options).unwrap();
            writer.write_all(b"hi there").unwrap();
            writer.finish().unwrap();
        }
        let dest = root.join("out");
        let mut progress = Vec::new();
        extract_zip(&archive_path, &dest, |p| progress.push(p)).unwrap();

        let extracted = std::fs::read_to_string(dest.join("topdir").join("bin").join("hello.txt")).unwrap();
        assert_eq!(extracted, "hi there");
        assert_eq!(*progress.last().unwrap(), 1.0);

        let sub = single_subdir(&dest).unwrap();
        assert_eq!(sub.file_name().unwrap(), "topdir");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn extract_tar_gz_reproduces_file_contents() {
        let root = scratch_dir("targz");
        std::fs::create_dir_all(&root).unwrap();
        let archive_path = root.join("fixture.tar.gz");
        {
            let file = File::create(&archive_path).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let content = b"interpreter placeholder";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, "python/python.exe", &content[..]).unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let dest = root.join("out");
        let mut progress = Vec::new();
        extract_tar_gz(&archive_path, &dest, |p| progress.push(p)).unwrap();

        let extracted = std::fs::read(dest.join("python").join("python.exe")).unwrap();
        assert_eq!(extracted, b"interpreter placeholder");
        assert!(!progress.is_empty());
        assert!((progress.last().unwrap() - 1.0).abs() < 1e-9);

        std::fs::remove_dir_all(&root).unwrap();
    }
}
