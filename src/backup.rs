//! Incremental backup of an archive folder to another folder (USB drive,
//! NAS, second disk). The archive is append-only, so after the first run a
//! backup copies only the new tail of each journal segment and message pack.
//! The backup is itself a complete archive that can be opened directly.
//!
//! Safe while the archive is in use: journal sizes are read first and only
//! that much is copied, then packs are copied in full, so every message the
//! copied journal mentions is in the copied packs.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

#[derive(Debug, Default, Clone, Serialize)]
pub struct BackupReport {
    pub copied: u64,
    pub appended: u64,
    pub unchanged: u64,
    pub removed: u64,
    pub bytes: u64,
}

const TAIL_CHECK: u64 = 4096;

fn files_in(dir: &Path, ext: &str) -> Result<Vec<PathBuf>> {
    let mut v = Vec::new();
    if dir.is_dir() {
        for e in fs::read_dir(dir)? {
            let p = e?.path();
            if p.is_file() && p.extension().is_some_and(|x| x == ext) {
                v.push(p);
            }
        }
    }
    v.sort();
    Ok(v)
}

fn read_range(path: &Path, off: u64, len: u64) -> Result<Vec<u8>> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(off))?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Copy `len` bytes of `src` to `dst`, appending when `dst` is a prefix.
fn sync_file(src: &Path, dst: &Path, len: u64, append_only: bool, r: &mut BackupReport) -> Result<()> {
    let have = fs::metadata(dst).map(|m| m.len()).ok();
    if let Some(h) = have {
        if h == len && (append_only || same_mtime(src, dst)) {
            let check = h.min(TAIL_CHECK);
            if !append_only || read_range(src, h - check, check)? == read_range(dst, h - check, check)? {
                r.unchanged += 1;
                return Ok(());
            }
        }
        if append_only && h < len {
            let check = h.min(TAIL_CHECK);
            if read_range(src, h - check, check)? == read_range(dst, h - check, check)? {
                let mut from = File::open(src)?;
                from.seek(SeekFrom::Start(h))?;
                let mut to = OpenOptions::new().append(true).open(dst)?;
                let n = std::io::copy(&mut from.take(len - h), &mut to)?;
                to.sync_all()?;
                r.appended += 1;
                r.bytes += n;
                return Ok(());
            }
        }
    }
    // Full copy through a temporary file.
    let tmp = dst.with_extension("nea-tmp");
    {
        let mut from = File::open(src)?.take(len);
        let mut to = File::create(&tmp)?;
        let n = std::io::copy(&mut from, &mut to)?;
        to.sync_all()?;
        r.bytes += n;
    }
    fs::rename(&tmp, dst)?;
    if let Ok(m) = fs::metadata(src).and_then(|m| m.modified()) {
        let _ = File::options().write(true).open(dst).and_then(|f| f.set_modified(m));
    }
    r.copied += 1;
    Ok(())
}

fn same_mtime(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a).and_then(|m| m.modified()), fs::metadata(b).and_then(|m| m.modified())) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

pub fn backup(root: &Path, dest: &Path, mut progress: impl FnMut(&BackupReport)) -> Result<BackupReport> {
    if !root.join("archive.json").is_file() {
        bail!("{} is not an archive", root.display());
    }
    let (sr, dr) = (fs::canonicalize(root)?, fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf()));
    if dr.starts_with(&sr) || sr.starts_with(&dr) {
        bail!("the backup folder must not be inside the archive (or the other way round)");
    }
    if dest.join("archive.json").is_file() {
        let a: serde_json::Value = serde_json::from_slice(&fs::read(root.join("archive.json"))?)?;
        let b: serde_json::Value = serde_json::from_slice(&fs::read(dest.join("archive.json"))?)?;
        if a.get("id") != b.get("id") {
            bail!("{} holds a different archive; choose an empty folder or this archive's earlier backup", dest.display());
        }
    } else if dest.is_dir() && fs::read_dir(dest)?.next().is_some() {
        bail!("{} is not empty; choose an empty folder for the first backup", dest.display());
    }
    let mut r = BackupReport::default();
    for sub in ["journal", "messages"] {
        fs::create_dir_all(dest.join(sub)).with_context(|| format!("creating {}", dest.join(sub).display()))?;
    }

    // 1. Journal, up to the sizes seen now.
    let journal: Vec<(PathBuf, u64)> = files_in(&root.join("journal"), "jsonl")?
        .into_iter()
        .map(|p| {
            let n = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            (p, n)
        })
        .collect();
    // 2. Packs, in full (they are at least as new as the journal).
    let packs = files_in(&root.join("messages"), "nmp")?;
    for p in &packs {
        let len = fs::metadata(p)?.len();
        sync_file(p, &dest.join("messages").join(p.file_name().unwrap()), len, true, &mut r)?;
        progress(&r);
    }
    for (p, len) in &journal {
        sync_file(p, &dest.join("journal").join(p.file_name().unwrap()), *len, true, &mut r)?;
    }
    for name in ["profiles.json", "archive.json"] {
        let p = root.join(name);
        if p.is_file() {
            let len = fs::metadata(&p)?.len();
            sync_file(&p, &dest.join(name), len, false, &mut r)?;
        }
    }
    // Packs removed by compaction go from the backup too, last, so the
    // backup is never missing bytes its journal points at.
    let keep: std::collections::HashSet<_> = packs.iter().filter_map(|p| p.file_name().map(|n| n.to_os_string())).collect();
    for p in files_in(&dest.join("messages"), "nmp")? {
        if !keep.contains(p.file_name().unwrap()) {
            fs::remove_file(&p)?;
            r.removed += 1;
        }
    }
    progress(&r);
    Ok(r)
}
