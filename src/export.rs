//! Export a set of messages (usually a search result) to standard formats.
//! Messages are written byte-for-byte as archived.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::Result;

use crate::archive::Archive;
use crate::parse;

#[derive(Debug, Default, serde::Serialize)]
pub struct ExportStats {
    pub written: u64,
    pub failed: u64,
}

pub fn sanitize(name: &str, max: usize) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    s = s.trim().trim_end_matches('.').to_string();
    if s.chars().count() > max {
        s = s.chars().take(max).collect();
    }
    // Windows reserved names
    let upper = s.to_uppercase();
    if ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "LPT1", "LPT2"].contains(&upper.as_str()) {
        s.push('_');
    }
    if s.is_empty() {
        "_".into()
    } else {
        s
    }
}

/// One .eml per message, in folders mirroring the first archived location.
pub fn to_eml_dir(archive: &Archive, ids: &[String], out: &Path) -> Result<ExportStats> {
    let mut st = ExportStats::default();
    for id in ids {
        let res = (|| -> Result<()> {
            let raw = archive.read_raw(id)?;
            let p = parse::parse(&raw);
            let locs = archive.catalog.locations(id)?;
            let mut dir = out.to_path_buf();
            if let Some((src, folder)) = locs.first() {
                dir.push(sanitize(src, 80));
                for part in folder.split('/').filter(|s| !s.is_empty()) {
                    dir.push(sanitize(part, 80));
                }
            }
            fs::create_dir_all(&dir)?;
            let date = chrono::DateTime::from_timestamp(p.date, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "nodate".into());
            let subj = if p.subject.is_empty() { "(no subject)".to_string() } else { p.subject.clone() };
            let name = format!("{} {} [{}].eml", date, sanitize(&subj, 80), &id[..8]);
            fs::write(dir.join(name), &raw)?;
            Ok(())
        })();
        match res {
            Ok(()) => st.written += 1,
            Err(_) => st.failed += 1,
        }
    }
    Ok(st)
}

/// A single mboxrd file.
pub fn to_mbox(archive: &Archive, ids: &[String], out: &Path) -> Result<ExportStats> {
    let mut st = ExportStats::default();
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut w = BufWriter::with_capacity(1 << 20, File::create(out)?);
    for id in ids {
        let Ok(raw) = archive.read_raw(id) else {
            st.failed += 1;
            continue;
        };
        let p = parse::parse(&raw);
        let sender = p.from.first().map(|a| a.email.clone()).filter(|e| !e.is_empty()).unwrap_or_else(|| "MAILER-DAEMON".into());
        let date = chrono::DateTime::from_timestamp(p.date.max(0), 0).unwrap_or_default();
        writeln!(w, "From {} {}", sender, date.format("%a %b %e %H:%M:%S %Y"))?;
        write_mboxrd_body(&mut w, &raw)?;
        st.written += 1;
    }
    w.flush()?;
    Ok(st)
}

fn write_mboxrd_body(w: &mut impl Write, raw: &[u8]) -> std::io::Result<()> {
    for line in raw.split_inclusive(|&b| b == b'\n') {
        let gt = line.iter().take_while(|&&b| b == b'>').count();
        if line[gt..].starts_with(b"From ") {
            w.write_all(b">")?;
        }
        w.write_all(line)?;
    }
    if !raw.ends_with(b"\n") {
        w.write_all(b"\n")?;
    }
    w.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::mbox::MboxReader;

    #[test]
    fn mboxrd_roundtrip() {
        let raw = b"Subject: x\n\nFrom the top\n>From quoted\nend\n".to_vec();
        let mut buf = Vec::new();
        buf.extend_from_slice(b"From a@b Mon Jan  1 00:00:00 2024\n");
        write_mboxrd_body(&mut buf, &raw).unwrap();
        buf.extend_from_slice(b"From a@b Mon Jan  1 00:00:00 2024\n");
        write_mboxrd_body(&mut buf, &raw).unwrap();
        let msgs: Vec<Vec<u8>> = MboxReader::new(&buf[..]).map(|r| r.unwrap()).collect();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], raw);
        assert_eq!(msgs[1], raw);
    }

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("a/b:c?", 80), "a_b_c_");
        assert_eq!(sanitize("CON", 80), "CON_");
        assert_eq!(sanitize("  ", 80), "_");
    }
}
