//! POP3 archiving (RFC 1939, TLS on 995 or plaintext for tests).
//!
//! POP3 has no folders and no flags. Each run lists the mailbox, reads only
//! the headers (TOP n 0) of each message, and downloads (RETR) the ones the
//! archive does not already hold. With a delete rule, messages proven to be
//! archived (by SHA-256 of the downloaded bytes, or Message-ID + size) and
//! older than the rule's age are deleted (DELE) when the session ends.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::archive::{ingest, sha256_hex, Archive, ImportStats};
use crate::import::mbox::header_value;
use crate::rules::{DateWindow, DeleteRule};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pop3Config {
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(skip)]
    pub password: String,
    #[serde(default)]
    pub src: Option<String>,
    #[serde(default)]
    pub plaintext: bool,
    #[serde(default)]
    pub window: DateWindow,
    #[serde(default)]
    pub delete: Option<DeleteRule>,
}

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

struct Pop3 {
    r: BufReader<Box<dyn Stream>>,
}

impl Pop3 {
    fn connect(cfg: &Pop3Config) -> Result<Self> {
        let tcp = TcpStream::connect((cfg.host.as_str(), cfg.port)).with_context(|| format!("connecting to {}:{}", cfg.host, cfg.port))?;
        tcp.set_read_timeout(Some(Duration::from_secs(120)))?;
        let s: Box<dyn Stream> = if cfg.plaintext {
            Box::new(tcp)
        } else {
            let connector = rustls_connector::RustlsConnector::new_with_native_certs().unwrap_or_default();
            Box::new(connector.connect(&cfg.host, tcp).map_err(|e| anyhow::anyhow!("TLS handshake with {}: {e:?}", cfg.host))?)
        };
        let mut p = Pop3 { r: BufReader::new(s) };
        p.status_line().context("POP3 greeting")?;
        Ok(p)
    }

    fn line(&mut self) -> Result<String> {
        let mut buf = Vec::new();
        let n = self.r.read_until(b'\n', &mut buf)?;
        if n == 0 {
            bail!("connection closed");
        }
        Ok(String::from_utf8_lossy(&buf).trim_end_matches(['\r', '\n']).to_string())
    }

    fn status_line(&mut self) -> Result<String> {
        let l = self.line()?;
        if let Some(rest) = l.strip_prefix("+OK") {
            Ok(rest.trim().to_string())
        } else {
            bail!("server said: {l}")
        }
    }

    fn cmd(&mut self, c: &str) -> Result<String> {
        let w = self.r.get_mut();
        w.write_all(c.as_bytes())?;
        w.write_all(b"\r\n")?;
        w.flush()?;
        self.status_line().with_context(|| c.split(' ').next().unwrap_or("").to_string())
    }

    /// Read a dot-terminated multi-line response (dot-unstuffed, CRLF kept).
    fn multiline(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let mut buf = Vec::new();
            if self.r.read_until(b'\n', &mut buf)? == 0 {
                bail!("connection closed mid-response");
            }
            if buf == b".\r\n" || buf == b".\n" {
                break;
            }
            if buf.starts_with(b"..") {
                out.extend_from_slice(&buf[1..]);
            } else {
                out.extend_from_slice(&buf);
            }
        }
        Ok(out)
    }
}

pub fn import(archive: &mut Archive, cfg: &Pop3Config, mut progress: impl FnMut(&ImportStats)) -> Result<ImportStats> {
    cfg.window.validate()?;
    let src = cfg.src.clone().unwrap_or_else(|| format!("{}@{}", cfg.user, cfg.host).to_lowercase());
    let mut p = Pop3::connect(cfg)?;
    p.cmd(&format!("USER {}", cfg.user))?;
    p.cmd(&format!("PASS {}", cfg.password)).context("POP3 login failed")?;
    p.cmd("LIST")?;
    let listing = String::from_utf8_lossy(&p.multiline()?).to_string();
    let msgs: Vec<(u32, i64)> = listing
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect();

    let mut stats = ImportStats::default();
    // (msg number, archived id, message date)
    let mut archived: Vec<(u32, String, i64)> = Vec::new();
    let mut batch = Vec::new();
    let mut batch_ids: Vec<(u32, String, i64)> = Vec::new();
    for (n, size) in &msgs {
        p.cmd(&format!("TOP {n} 0"))?;
        let head = p.multiline()?;
        let date = mail_parser::MessageParser::default().parse(&head).and_then(|m| m.date().map(|d| d.to_timestamp())).unwrap_or(0);
        if !cfg.window.is_empty() && !cfg.window.contains(date) {
            stats.skipped += 1;
            continue;
        }
        let mid = header_value(&head, "Message-ID").map(|m| m.trim().trim_matches(['<', '>']).to_string());
        if let Some(m) = mid.filter(|m| !m.is_empty()) {
            if let Some(id) = archive.find_by_msgid_size(&m, *size)? {
                stats.record(archive.add_location_existing(&id, &src, "INBOX")?);
                archived.push((*n, id, date));
                continue;
            }
        }
        p.cmd(&format!("RETR {n}"))?;
        let raw = p.multiline()?;
        batch_ids.push((*n, sha256_hex(&raw), date));
        batch.push(Ok((raw, src.clone(), vec!["INBOX".to_string()])));
        if batch.len() >= 200 {
            let window = archive.window.take();
            ingest(archive, std::mem::take(&mut batch).into_iter(), &mut stats, |_| {})?;
            archive.window = window;
            archived.append(&mut batch_ids);
            progress(&stats);
        }
    }
    if !batch.is_empty() {
        let window = archive.window.take();
        ingest(archive, batch.into_iter(), &mut stats, |_| {})?;
        archive.window = window;
        archived.append(&mut batch_ids);
    }
    archive.commit()?;

    if let Some(rule) = &cfg.delete {
        let cutoff = rule.cutoff();
        for (n, id, date) in &archived {
            if *date > 0 && *date < cutoff && archive.catalog.has_message(id)? {
                p.cmd(&format!("DELE {n}"))?;
                stats.deleted += 1;
            }
        }
    }
    // QUIT makes DELE permanent.
    p.cmd("QUIT")?;
    archive.record_import("pop3", &format!("{src} ({})", cfg.host), &stats)?;
    archive.commit()?;
    progress(&stats);
    Ok(stats)
}
