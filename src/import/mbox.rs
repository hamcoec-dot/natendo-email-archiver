//! MBOX import: single files, Google Takeout, Thunderbird profile folders.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::archive::{ingest, Archive, ImportStats};

/// Streaming reader yielding one raw message per item (mboxrd unescaped).
pub struct MboxReader<R: BufRead> {
    r: R,
    next_from: bool,
    line: Vec<u8>,
    done: bool,
    pub count: u64,
}

impl MboxReader<BufReader<File>> {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self::new(BufReader::with_capacity(1 << 20, File::open(path)?)))
    }
}

fn is_from_line(l: &[u8]) -> bool {
    // "From sender@x Tue Mar 18 10:01:00 2008" — require a sender token and a
    // digit, to avoid splitting on an unescaped "From here on..." in a body.
    if !l.starts_with(b"From ") {
        return false;
    }
    let s = String::from_utf8_lossy(l);
    s.split_whitespace().count() >= 3 && s.chars().any(|c| c.is_ascii_digit())
}

impl<R: BufRead> MboxReader<R> {
    pub fn new(r: R) -> Self {
        Self { r, next_from: false, line: Vec::new(), done: false, count: 0 }
    }
}

impl<R: BufRead> Iterator for MboxReader<R> {
    type Item = std::io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        // Skip to the first From_ line (start of file).
        if !self.next_from {
            loop {
                self.line.clear();
                match self.r.read_until(b'\n', &mut self.line) {
                    Ok(0) => {
                        self.done = true;
                        return None;
                    }
                    Ok(_) if is_from_line(&self.line) => break,
                    Ok(_) => continue,
                    Err(e) => return Some(Err(e)),
                }
            }
        }
        self.next_from = false;
        let mut msg = Vec::new();
        let mut prev_blank = true;
        loop {
            self.line.clear();
            match self.r.read_until(b'\n', &mut self.line) {
                Ok(0) => {
                    self.done = true;
                    break;
                }
                Ok(_) => {
                    if prev_blank && is_from_line(&self.line) {
                        self.next_from = true;
                        break;
                    }
                    let l = &self.line;
                    prev_blank = l == b"\n" || l == b"\r\n";
                    // mboxrd: ">From " / ">>From " -> strip one '>'
                    let gt = l.iter().take_while(|&&b| b == b'>').count();
                    if gt > 0 && l[gt..].starts_with(b"From ") {
                        msg.extend_from_slice(&l[1..]);
                    } else {
                        msg.extend_from_slice(l);
                    }
                }
                Err(e) => return Some(Err(e)),
            }
        }
        // The blank line before the next From_ belongs to the separator.
        if msg.ends_with(b"\r\n\r\n") {
            msg.truncate(msg.len() - 2);
        } else if msg.ends_with(b"\n\n") {
            msg.truncate(msg.len() - 1);
        }
        self.count += 1;
        Some(Ok(msg))
    }
}

/// Value of a header in the raw header block (unfolded), if present.
pub fn header_value(raw: &[u8], name: &str) -> Option<String> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n"))
        .unwrap_or(raw.len().min(64 * 1024));
    let head = String::from_utf8_lossy(&raw[..end]);
    let mut val: Option<String> = None;
    for line in head.lines() {
        if let Some(v) = val.as_mut() {
            if line.starts_with(' ') || line.starts_with('\t') {
                v.push(' ');
                v.push_str(line.trim());
                continue;
            }
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case(name) {
                val = Some(v.trim().to_string());
            }
        }
    }
    val
}

/// Find mbox files under a path. Returns (file, folder name).
pub fn find_mboxes(path: &Path) -> Vec<(PathBuf, String)> {
    if path.is_file() {
        let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        return vec![(path.to_path_buf(), stem)];
    }
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(path).sort_by_file_name().into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_file() {
            continue;
        }
        let p = e.path();
        let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase());
        let candidate = match ext.as_deref() {
            Some("mbox") | Some("mbx") => true,
            None => {
                // Thunderbird stores folders as extensionless mbox files.
                let mut head = [0u8; 5];
                File::open(p).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"From "
            }
            _ => false,
        };
        if !candidate {
            continue;
        }
        let rel = p.strip_prefix(path).unwrap_or(p);
        let mut parts: Vec<String> =
            rel.with_extension("").components().map(|c| c.as_os_str().to_string_lossy().trim_end_matches(".sbd").to_string()).collect();
        // A whole Thunderbird profile: drop the Mail/ or ImapMail/ prefix.
        if parts.len() > 2 && (parts[0] == "Mail" || parts[0] == "ImapMail") {
            parts.remove(0);
        }
        let folder = parts.join("/");
        out.push((p.to_path_buf(), folder));
    }
    out
}

pub fn import(archive: &mut Archive, path: &Path, src: &str, progress: impl FnMut(&ImportStats)) -> Result<ImportStats> {
    let mut stats = ImportStats::default();
    let files = find_mboxes(path);
    let src = src.to_string();
    let items = files.into_iter().flat_map(move |(file, folder)| {
        let src = src.clone();
        let label = file.display().to_string();
        let reader: Box<dyn Iterator<Item = std::io::Result<Vec<u8>>> + Send> = match MboxReader::open(&file) {
            Ok(r) => Box::new(r),
            Err(e) => Box::new(std::iter::once(Err(e))),
        };
        reader.enumerate().map(move |(i, r)| match r {
            Ok(raw) => {
                // Google Takeout: one mbox, labels in a header.
                let folders = match header_value(&raw, "X-Gmail-Labels") {
                    Some(l) => l.split(',').map(|s| s.trim().trim_matches('"').to_string()).filter(|s| !s.is_empty()).collect(),
                    None => vec![folder.clone()],
                };
                Ok((raw, src.clone(), folders))
            }
            Err(e) => Err(format!("{label} message {}: {e}", i + 1)),
        })
    });
    ingest(archive, items, &mut stats, progress)?;
    archive.record_import("mbox", &path.display().to_string(), &stats)?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_unescapes() {
        let data = b"From a@b.c Mon Jan  1 00:00:00 2024\nSubject: one\n\nHello\n>From the start\n\nFrom b@c.d Tue Jan  2 00:00:00 2024\nSubject: two\n\nFrom here on, no split\nBye\n";
        let msgs: Vec<Vec<u8>> = MboxReader::new(&data[..]).map(|r| r.unwrap()).collect();
        assert_eq!(msgs.len(), 2);
        assert_eq!(String::from_utf8_lossy(&msgs[0]), "Subject: one\n\nHello\nFrom the start\n");
        assert!(String::from_utf8_lossy(&msgs[1]).contains("From here on, no split"));
    }

    #[test]
    fn header_lookup_unfolds() {
        let raw = b"Subject: x\nX-Gmail-Labels: Inbox,\n Important\n\nbody";
        assert_eq!(header_value(raw, "x-gmail-labels").unwrap(), "Inbox, Important");
        assert!(header_value(raw, "missing").is_none());
    }
}
