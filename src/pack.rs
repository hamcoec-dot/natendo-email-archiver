//! Message packs: raw messages appended to large files instead of one file
//! per message. One fsync per commit instead of one per message, and a few
//! hundred files instead of millions, which matters most on USB drives and
//! network shares (SMB/NFS are slow at creating small files).
//!
//! Format (documented so mail is recoverable without this app):
//!
//! ```text
//! messages/pack-000001.nmp
//!   NEAMSG1 <sha256-hex> <length>\n
//!   <length bytes: the message exactly as received>\n
//!   NEAMSG1 ...
//! ```
//!
//! Packs are append-only; bytes once written never change.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};

pub const PACK_MAX: u64 = 128 * 1024 * 1024;
const MAGIC: &str = "NEAMSG1";

pub fn pack_dir(root: &Path) -> PathBuf {
    root.join("messages")
}

pub fn pack_path(root: &Path, n: u32) -> PathBuf {
    pack_dir(root).join(format!("pack-{n:06}.nmp"))
}

pub fn list_packs(root: &Path) -> Result<Vec<(u32, PathBuf)>> {
    let mut v = Vec::new();
    let dir = pack_dir(root);
    if !dir.exists() {
        return Ok(v);
    }
    for e in fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(n) = name.strip_prefix("pack-").and_then(|s| s.strip_suffix(".nmp")).and_then(|s| s.parse().ok()) {
            v.push((n, e.path()));
        }
    }
    v.sort();
    Ok(v)
}

pub struct PackWriter {
    root: PathBuf,
    num: u32,
    out: BufWriter<File>,
    len: u64,
    dirty: bool,
}

impl PackWriter {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(pack_dir(root))?;
        let last = list_packs(root)?.last().map(|p| p.0);
        let num = match last {
            Some(n) if fs::metadata(pack_path(root, n))?.len() < PACK_MAX => n,
            Some(n) => n + 1,
            None => 1,
        };
        let (out, len) = Self::open_num(root, num)?;
        Ok(Self { root: root.to_path_buf(), num, out, len, dirty: false })
    }

    fn open_num(root: &Path, n: u32) -> Result<(BufWriter<File>, u64)> {
        let p = pack_path(root, n);
        let mut f = OpenOptions::new().create(true).read(true).append(true).open(&p).with_context(|| format!("opening {}", p.display()))?;
        let mut len = f.metadata()?.len();
        // Terminate a torn tail from a crash so the next header starts a line.
        if len > 0 {
            f.seek(SeekFrom::Start(len - 1))?;
            let mut b = [0u8; 1];
            f.read_exact(&mut b)?;
            if b[0] != b'\n' {
                f.write_all(b"\n")?;
                len += 1;
            }
        }
        Ok((BufWriter::with_capacity(4 << 20, f), len))
    }

    /// Append a message; returns (pack number, offset of the message bytes).
    pub fn append(&mut self, id: &str, raw: &[u8]) -> Result<(u32, u64)> {
        if self.len > 0 && self.len + raw.len() as u64 > PACK_MAX {
            self.sync()?;
            self.num += 1;
            let (out, len) = Self::open_num(&self.root, self.num)?;
            self.out = out;
            self.len = len;
        }
        let header = format!("{MAGIC} {id} {}\n", raw.len());
        self.out.write_all(header.as_bytes())?;
        let off = self.len + header.len() as u64;
        self.out.write_all(raw)?;
        self.out.write_all(b"\n")?;
        self.len = off + raw.len() as u64 + 1;
        self.dirty = true;
        Ok((self.num, off))
    }

    /// Make appended messages durable. Must happen before the journal
    /// references them.
    pub fn sync(&mut self) -> Result<()> {
        if self.dirty {
            self.out.flush()?;
            self.out.get_ref().sync_data()?;
            self.dirty = false;
        }
        Ok(())
    }
}

impl Drop for PackWriter {
    fn drop(&mut self) {
        let _ = self.sync();
    }
}

/// Shared, thread-safe positional reader with cached handles.
#[derive(Clone)]
pub struct PackReader {
    root: PathBuf,
    handles: Arc<Mutex<HashMap<u32, Arc<File>>>>,
}

#[cfg(windows)]
fn read_exact_at(f: &File, mut buf: &mut [u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_read(buf, off)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)
}

impl PackReader {
    pub fn new(root: &Path) -> Self {
        Self { root: root.to_path_buf(), handles: Arc::new(Mutex::new(HashMap::new())) }
    }

    fn handle(&self, n: u32) -> Result<Arc<File>> {
        let mut h = self.handles.lock().unwrap();
        if let Some(f) = h.get(&n) {
            return Ok(f.clone());
        }
        let p = pack_path(&self.root, n);
        let f = Arc::new(File::open(&p).with_context(|| format!("opening {}", p.display()))?);
        if h.len() > 64 {
            h.clear();
        }
        h.insert(n, f.clone());
        Ok(f)
    }

    pub fn read(&self, n: u32, off: u64, len: u64) -> Result<Vec<u8>> {
        let f = self.handle(n)?;
        let mut buf = vec![0u8; len as usize];
        if let Err(e) = read_exact_at(&f, &mut buf, off) {
            // The pack may have grown since the handle was cached (another
            // machine appended); reopen once.
            self.handles.lock().unwrap().remove(&n);
            let f = self.handle(n)?;
            read_exact_at(&f, &mut buf, off).with_context(|| format!("reading pack {n} at {off}: {e}"))?;
        }
        Ok(buf)
    }
}

/// Scan a pack, yielding (id, offset, len) for every intact record. Skips
/// damaged regions by searching for the next record header.
pub fn scan(path: &Path) -> Result<Vec<(String, u64, u64)>> {
    let mut f = BufReader::with_capacity(1 << 20, File::open(path)?);
    let total = f.get_ref().metadata()?.len();
    let mut out = Vec::new();
    let mut pos = 0u64;
    let mut line = Vec::new();
    while pos < total {
        line.clear();
        let n = f.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        let header = String::from_utf8_lossy(&line);
        let parts: Vec<&str> = header.trim_end().split(' ').collect();
        if parts.len() == 3 && parts[0] == MAGIC && parts[1].len() == 64 {
            if let Ok(len) = parts[2].parse::<u64>() {
                let off = pos + n as u64;
                if off + len < total && byte_at(&mut f, off + len)? == b'\n' {
                    out.push((parts[1].to_string(), off, len));
                    f.seek(SeekFrom::Start(off + len + 1))?;
                    pos = off + len + 1;
                    continue;
                }
            }
        }
        pos += n as u64;
        f.seek(SeekFrom::Start(pos))?;
    }
    Ok(out)
}

fn byte_at(f: &mut BufReader<File>, at: u64) -> Result<u8> {
    let mut b = [0u8; 1];
    f.seek(SeekFrom::Start(at))?;
    f.read_exact(&mut b)?;
    Ok(b[0])
}

pub fn read_record(path: &Path, off: u64, len: u64) -> Result<Vec<u8>> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(off))?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn check_id(id: &str) -> Result<()> {
    if id.len() != 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("invalid message id");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_read_scan_rotate() {
        let d = tempfile::tempdir().unwrap();
        let id1 = "a".repeat(64);
        let id2 = "b".repeat(64);
        let (n1, o1, n2, o2);
        {
            let mut w = PackWriter::open(d.path()).unwrap();
            (n1, o1) = w.append(&id1, b"hello\nworld").unwrap();
            (n2, o2) = w.append(&id2, b"NEAMSG1 fake header inside body\n").unwrap();
            w.sync().unwrap();
        }
        let r = PackReader::new(d.path());
        assert_eq!(r.read(n1, o1, 11).unwrap(), b"hello\nworld");
        assert_eq!(r.read(n2, o2, 32).unwrap(), b"NEAMSG1 fake header inside body\n");
        let recs = scan(&pack_path(d.path(), 1)).unwrap();
        assert_eq!(recs, vec![(id1.clone(), o1, 11), (id2.clone(), o2, 32)]);
        // A torn tail is skipped and later appends still scan.
        let p = pack_path(d.path(), 1);
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"NEAMSG1 cccc 999\npartial").unwrap();
        drop(f);
        let mut w = PackWriter::open(d.path()).unwrap();
        let (_, o3) = w.append(&"d".repeat(64), b"after").unwrap();
        w.sync().unwrap();
        let recs = scan(&p).unwrap();
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[2].1, o3);
    }
}
