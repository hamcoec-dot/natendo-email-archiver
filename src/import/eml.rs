//! Folder-of-.eml import. Also the MailStore Home migration path: MailStore's
//! "Export Email to a Directory (File System)" writes one .eml per message in
//! a tree that mirrors the archive's folders (often starting with the account
//! address), which this importer turns back into sources and folders.

use std::path::Path;

use anyhow::Result;

use crate::archive::{ingest, Archive, ImportStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// Generic folder of .eml files.
    Files,
    /// MailStore Home export: first folder level is the source when it looks
    /// like an account (contains '@').
    MailStore,
}

/// Split a relative folder path into (source, folder) for the given flavor.
pub fn source_and_folder(rel_dir: &str, default_src: &str, flavor: Flavor) -> (String, String) {
    let parts: Vec<&str> = rel_dir.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if flavor == Flavor::MailStore {
        if let Some(first) = parts.first() {
            if first.contains('@') {
                return (first.to_lowercase(), parts[1..].join("/"));
            }
        }
    }
    (default_src.to_string(), parts.join("/"))
}

pub fn import(
    archive: &mut Archive,
    dir: &Path,
    src: &str,
    flavor: Flavor,
    progress: impl FnMut(&ImportStats),
) -> Result<ImportStats> {
    let mut stats = ImportStats::default();
    let mut skipped_msg = 0u64;
    let root = dir.to_path_buf();
    let src = src.to_string();
    let walker = walkdir::WalkDir::new(dir).sort_by_file_name().into_iter();
    let msg_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mc = msg_count.clone();
    let items = walker.filter_map(move |e| {
        let e = match e {
            Ok(e) => e,
            Err(err) => return Some(Err(format!("walk: {err}"))),
        };
        if !e.file_type().is_file() {
            return None;
        }
        let p = e.path();
        let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
        if ext == "msg" {
            mc.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return None;
        }
        if ext != "eml" {
            return None;
        }
        let rel = p.parent().and_then(|d| d.strip_prefix(&root).ok()).map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
        let (s, folder) = source_and_folder(&rel, &src, flavor);
        Some(match std::fs::read(p) {
            Ok(raw) if raw.is_empty() => Err(format!("{}: empty file", p.display())),
            Ok(raw) => Ok((raw, s, vec![folder])),
            Err(err) => Err(format!("{}: {err}", p.display())),
        })
    });
    ingest(archive, items, &mut stats, progress)?;
    skipped_msg += msg_count.load(std::sync::atomic::Ordering::Relaxed);
    if skipped_msg > 0 {
        stats.fail(format!(
            "{skipped_msg} Outlook .msg files were skipped: .msg import is not built yet. Re-export from MailStore as EML."
        ));
        stats.failed += skipped_msg - 1;
    }
    let kind = if flavor == Flavor::MailStore { "mailstore" } else { "eml" };
    archive.record_import(kind, &dir.display().to_string(), &stats)?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailstore_paths() {
        assert_eq!(
            source_and_folder("alex@example.com/INBOX/Receipts", "mailstore", Flavor::MailStore),
            ("alex@example.com".into(), "INBOX/Receipts".into())
        );
        assert_eq!(source_and_folder("Archive/2019", "mailstore", Flavor::MailStore), ("mailstore".into(), "Archive/2019".into()));
        assert_eq!(source_and_folder("a\\b", "files", Flavor::Files), ("files".into(), "a/b".into()));
    }
}
