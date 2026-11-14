//! Append-only journal: the archive's portable, network-safe catalog.
//!
//! Every fact about the archive that is not inside a message file (where a
//! message was found, IMAP sync positions) is a JSON line appended to
//! `journal/NNNNNN.jsonl`. Lines are never rewritten, so the folder can live
//! on a USB stick or a NAS share and be read by several machines while one
//! writer appends. Each machine replays new lines into its local cache.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const SEGMENT_MAX: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Entry {
    /// Message `id` (sha256 of its bytes) was found in `source`/`folder`.
    /// `p` = [pack, offset, length] of the message bytes, present on the
    /// entry that first archives a message.
    Msg {
        id: String,
        src: String,
        folder: String,
        at: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        p: Option<[u64; 3]>,
    },
    /// IMAP sync position for a folder.
    Sync { src: String, folder: String, uidvalidity: u32, last_uid: u32 },
    /// Record of an import run, for the history panel.
    Import { kind: String, path: String, at: i64, added: u64, dupes: u64, failed: u64 },
}

pub fn journal_dir(root: &Path) -> PathBuf {
    root.join("journal")
}

pub fn segments(root: &Path) -> Result<Vec<(u32, PathBuf)>> {
    let dir = journal_dir(root);
    let mut v = Vec::new();
    if !dir.exists() {
        return Ok(v);
    }
    for e in fs::read_dir(&dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(n) = name.strip_suffix(".jsonl").and_then(|s| s.parse::<u32>().ok()) {
            v.push((n, e.path()));
        }
    }
    v.sort();
    Ok(v)
}

fn seg_path(root: &Path, n: u32) -> PathBuf {
    journal_dir(root).join(format!("{n:06}.jsonl"))
}

pub struct JournalWriter {
    root: PathBuf,
    seg: u32,
    file: File,
    len: u64,
    buf: Vec<u8>,
}

impl JournalWriter {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(journal_dir(root))?;
        let seg = segments(root)?.last().map(|s| s.0).unwrap_or(1);
        let path = seg_path(root, seg);
        let mut file = OpenOptions::new().create(true).read(true).append(true).open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut len = file.metadata()?.len();
        // A crash can leave a partial last line; terminate it so the next
        // entry starts clean (readers skip the unparseable fragment).
        if len > 0 {
            file.seek(SeekFrom::Start(len - 1))?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
                len += 1;
            }
        }
        Ok(Self { root: root.to_path_buf(), seg, file, len, buf: Vec::new() })
    }

    pub fn append(&mut self, e: &Entry) -> Result<()> {
        serde_json::to_writer(&mut self.buf, e)?;
        self.buf.push(b'\n');
        if self.buf.len() > 1 << 20 {
            self.flush()?;
        }
        Ok(())
    }

    /// Write buffered entries and fsync. Called at each commit point.
    pub fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        if self.len > 0 && self.len + self.buf.len() as u64 > SEGMENT_MAX {
            self.file.sync_all()?;
            self.seg += 1;
            self.file = OpenOptions::new().create(true).append(true).open(seg_path(&self.root, self.seg))?;
            self.len = 0;
        }
        self.file.write_all(&self.buf)?;
        self.file.sync_data()?;
        self.len += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }
}

impl Drop for JournalWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// Read complete entries of segment `path` starting at byte `offset`.
/// Returns each entry with the offset just past its line, and the offset
/// just past the last complete line.
pub fn read_from(path: &Path, offset: u64) -> Result<(Vec<(Entry, u64)>, u64)> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut r = BufReader::with_capacity(1 << 20, f);
    let mut out = Vec::new();
    let mut pos = offset;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = r.read_until(b'\n', &mut line)?;
        if n == 0 || line.last() != Some(&b'\n') {
            break; // EOF or a line still being written by another machine
        }
        pos += n as u64;
        if let Ok(e) = serde_json::from_slice::<Entry>(&line) {
            out.push((e, pos));
        }
    }
    Ok((out, pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_replay_incrementally() {
        let dir = tempfile::tempdir().unwrap();
        let e1 = Entry::Msg { id: "a".into(), src: "s".into(), folder: "INBOX".into(), at: 1, p: Some([1, 20, 300]) };
        let e2 = Entry::Sync { src: "s".into(), folder: "INBOX".into(), uidvalidity: 7, last_uid: 42 };
        {
            let mut w = JournalWriter::open(dir.path()).unwrap();
            w.append(&e1).unwrap();
            w.flush().unwrap();
        }
        let segs = segments(dir.path()).unwrap();
        let (got, off) = read_from(&segs[0].1, 0).unwrap();
        assert_eq!(got.into_iter().map(|x| x.0).collect::<Vec<_>>(), vec![e1.clone()]);
        {
            let mut w = JournalWriter::open(dir.path()).unwrap();
            w.append(&e2).unwrap();
        }
        let (got2, _) = read_from(&segs[0].1, off).unwrap();
        assert_eq!(got2.into_iter().map(|x| x.0).collect::<Vec<_>>(), vec![e2]);
    }

    #[test]
    fn partial_line_is_ignored_then_repaired() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(journal_dir(dir.path())).unwrap();
        fs::write(seg_path(dir.path(), 1), b"{\"op\":\"msg\",\"id\":\"x\"").unwrap();
        let (got, off) = read_from(&seg_path(dir.path(), 1), 0).unwrap();
        assert!(got.is_empty());
        assert_eq!(off, 0);
        let mut w = JournalWriter::open(dir.path()).unwrap();
        w.append(&Entry::Msg { id: "b".into(), src: "s".into(), folder: "f".into(), at: 0, p: None }).unwrap();
        w.flush().unwrap();
        let (got, _) = read_from(&seg_path(dir.path(), 1), 0).unwrap();
        assert_eq!(got.len(), 1);
    }
}
