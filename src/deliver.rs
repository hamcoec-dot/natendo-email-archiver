//! Put archived mail back into a live mailbox: copy to an IMAP account
//! (APPEND, keeping folders, dates and read state) or send by email (SMTP).
//! Messages go out byte-for-byte as archived.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::archive::Archive;
use crate::import::mbox::header_value;
use crate::parse;

#[derive(Debug, Default, Clone, Serialize)]
pub struct DeliverStats {
    pub sent: u64,
    /// Already in the target folder (same Message-ID).
    pub skipped: u64,
    pub failed: u64,
    pub errors: Vec<String>,
}

impl DeliverStats {
    fn fail(&mut self, e: String) {
        self.failed += 1;
        if self.errors.len() < 50 {
            self.errors.push(e);
        }
    }
}

// ---------------------------------------------------------------- IMAP

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImapTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(skip)]
    pub password: String,
    /// Put everything into this one folder (created if missing).
    #[serde(default)]
    pub folder: Option<String>,
    /// Otherwise recreate each message's archived folder under this prefix
    /// (e.g. "Restored"); empty keeps the original folder names.
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub plaintext: bool,
}

fn msgid_of(raw: &[u8]) -> Option<String> {
    header_value(raw, "Message-ID").map(|m| m.trim().trim_matches(['<', '>']).to_lowercase()).filter(|m| !m.is_empty())
}

/// Copy messages into an IMAP mailbox. Messages whose Message-ID is already
/// in the target folder are skipped, so a restore can be re-run safely.
pub fn to_imap(archive: &Archive, ids: &[String], t: &ImapTarget, mut progress: impl FnMut(&DeliverStats)) -> Result<DeliverStats> {
    let mode = if t.plaintext { imap::ConnectionMode::Plaintext } else { imap::ConnectionMode::AutoTls };
    let client = imap::ClientBuilder::new(t.host.as_str(), t.port)
        .mode(mode)
        .tls_kind(imap::TlsKind::Rust)
        .connect()
        .with_context(|| format!("connecting to {}:{}", t.host, t.port))?;
    let mut s = client.login(&t.user, &t.password).map_err(|(e, _)| e).context("IMAP login failed")?;

    let names = s.list(None, Some("*")).context("LIST")?;
    let delim = names.iter().find_map(|n| n.delimiter().map(|d| d.to_string())).unwrap_or_else(|| "/".into());
    let mut existing_folders: HashSet<String> = names.iter().map(|n| n.name().to_string()).collect();
    drop(names);
    // target folder -> Message-IDs already there
    let mut present: HashMap<String, HashSet<String>> = HashMap::new();
    let mut st = DeliverStats::default();

    for (i, id) in ids.iter().enumerate() {
        let res = (|| -> Result<bool> {
            let raw = archive.read_raw(id)?;
            let dest = match &t.folder {
                Some(f) if !f.trim().is_empty() => f.trim().to_string(),
                _ => {
                    let orig = archive.catalog.locations(id)?.into_iter().next().map(|(_, f)| f).unwrap_or_else(|| "INBOX".into());
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(p) = t.prefix.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                        parts.push(p.to_string());
                    }
                    parts.extend(orig.split('/').filter(|x| !x.is_empty()).map(String::from));
                    parts.join(&delim)
                }
            };
            if !present.contains_key(&dest) {
                if !existing_folders.iter().any(|f| f.eq_ignore_ascii_case(&dest)) {
                    // Create parents too; servers differ on whether CREATE does.
                    let segs: Vec<&str> = dest.split(delim.as_str()).collect();
                    for k in 1..=segs.len() {
                        let name = segs[..k].join(&delim);
                        if !existing_folders.contains(&name) {
                            let _ = s.create(&name);
                            existing_folders.insert(name);
                        }
                    }
                }
                let mut ids_there = HashSet::new();
                let mb = s.select(&dest).with_context(|| format!("opening folder {dest}"))?;
                if mb.exists > 0 {
                    let uids = s.uid_search("ALL")?;
                    let mut v: Vec<u32> = uids.into_iter().collect();
                    v.sort_unstable();
                    for chunk in v.chunks(500) {
                        let set = chunk.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
                        for f in s.uid_fetch(&set, "BODY.PEEK[HEADER]")?.iter() {
                            if let Some(m) = f.header().and_then(msgid_of) {
                                ids_there.insert(m);
                            }
                        }
                    }
                }
                present.insert(dest.clone(), ids_there);
            }
            let there = present.get_mut(&dest).unwrap();
            if let Some(m) = msgid_of(&raw) {
                if !there.insert(m) {
                    return Ok(false);
                }
            }
            let p = parse::parse(&raw);
            let mut cmd = s.append(&dest, &raw);
            cmd.flag(imap::types::Flag::Seen);
            if p.date > 0 {
                if let Some(d) = chrono::DateTime::from_timestamp(p.date, 0) {
                    cmd.internal_date(d.fixed_offset());
                }
            }
            cmd.finish().with_context(|| format!("APPEND to {dest}"))?;
            Ok(true)
        })();
        match res {
            Ok(true) => st.sent += 1,
            Ok(false) => st.skipped += 1,
            Err(e) => st.fail(format!("{}: {e:#}", &id[..12.min(id.len())])),
        }
        if i % 50 == 0 {
            progress(&st);
        }
    }
    let _ = s.logout();
    progress(&st);
    Ok(st)
}

// ---------------------------------------------------------------- SMTP

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// TLS from the start (port 465).
    Tls,
    /// Upgrade with STARTTLS (ports 587 and 25). Required, never skipped.
    #[default]
    Starttls,
    /// No encryption (local test servers only).
    Plain,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SmtpTarget {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub security: Security,
    /// Login (optional for relays that don't need one).
    #[serde(default)]
    pub user: Option<String>,
    #[serde(skip)]
    pub password: String,
    /// Envelope sender (MAIL FROM).
    pub from: String,
    /// Recipients.
    pub to: Vec<String>,
}

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

struct Smtp {
    r: BufReader<Box<dyn Stream>>,
}

impl Smtp {
    fn reply(&mut self) -> Result<(u16, String)> {
        let mut text = String::new();
        loop {
            let mut line = String::new();
            if self.r.read_line(&mut line)? == 0 {
                bail!("SMTP connection closed");
            }
            let code: u16 = line.get(..3).and_then(|c| c.parse().ok()).with_context(|| format!("bad SMTP reply {line:?}"))?;
            text.push_str(line.get(4..).unwrap_or("").trim_end());
            text.push(' ');
            if line.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, text.trim().to_string()));
            }
        }
    }

    fn cmd(&mut self, c: &str, ok: &[u16]) -> Result<String> {
        let w = self.r.get_mut();
        w.write_all(c.as_bytes())?;
        w.write_all(b"\r\n")?;
        w.flush()?;
        let (code, text) = self.reply()?;
        if !ok.contains(&code) {
            let shown = if c.starts_with("AUTH") { "AUTH" } else { c };
            bail!("{shown}: server said {code} {text}");
        }
        Ok(text)
    }
}

fn tls(host: &str, tcp: TcpStream) -> Result<Box<dyn Stream>> {
    let connector = rustls_connector::RustlsConnector::new_with_native_certs().unwrap_or_default();
    Ok(Box::new(connector.connect(host, tcp).map_err(|e| anyhow::anyhow!("TLS handshake with {host}: {e:?}"))?))
}

fn connect_smtp(t: &SmtpTarget) -> Result<(Smtp, String)> {
    let tcp = TcpStream::connect((t.host.as_str(), t.port)).with_context(|| format!("connecting to {}:{}", t.host, t.port))?;
    tcp.set_read_timeout(Some(Duration::from_secs(120)))?;
    let stream: Box<dyn Stream> = if t.security == Security::Tls { tls(&t.host, tcp.try_clone()?)? } else { Box::new(tcp.try_clone()?) };
    let mut c = Smtp { r: BufReader::new(stream) };
    let (code, text) = c.reply()?;
    if code != 220 {
        bail!("SMTP greeting: {code} {text}");
    }
    let mut ehlo = c.cmd("EHLO natendo-archiver.local", &[250])?;
    if t.security == Security::Starttls {
        if !ehlo.to_uppercase().contains("STARTTLS") {
            bail!("{} does not offer STARTTLS; use port 465 (TLS) instead", t.host);
        }
        c.cmd("STARTTLS", &[220])?;
        c = Smtp { r: BufReader::new(tls(&t.host, tcp)?) };
        ehlo = c.cmd("EHLO natendo-archiver.local", &[250])?;
    }
    if let Some(user) = t.user.as_deref().filter(|u| !u.is_empty()) {
        use base64::Engine;
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let caps = ehlo.to_uppercase();
        if caps.contains("PLAIN") || !caps.contains("LOGIN") {
            c.cmd(&format!("AUTH PLAIN {}", b64(&format!("\0{user}\0{}", t.password))), &[235]).context("SMTP login failed")?;
        } else {
            c.cmd("AUTH LOGIN", &[334])?;
            c.cmd(&b64(user), &[334])?;
            c.cmd(&b64(&t.password), &[235]).context("SMTP login failed")?;
        }
    }
    Ok((c, ehlo))
}

/// CRLF line endings and SMTP dot-stuffing, ending with "\r\n.\r\n".
pub fn smtp_data(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 40 + 8);
    for line in raw.split_inclusive(|&b| b == b'\n') {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        if body.starts_with(b".") {
            out.push(b'.');
        }
        out.extend_from_slice(body);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

fn bad_addr(a: &str) -> bool {
    a.is_empty() || a.contains(['<', '>', '\r', '\n', ' ']) || !a.contains('@')
}

/// Send each message, unchanged, to the given recipients.
pub fn to_smtp(archive: &Archive, ids: &[String], t: &SmtpTarget, mut progress: impl FnMut(&DeliverStats)) -> Result<DeliverStats> {
    if bad_addr(&t.from) || t.to.is_empty() || t.to.iter().any(|a| bad_addr(a)) {
        bail!("sender and recipients must be plain email addresses");
    }
    let (mut c, ehlo) = connect_smtp(t)?;
    let size_ok = ehlo.to_uppercase().contains("SIZE");
    let mut st = DeliverStats::default();
    for (i, id) in ids.iter().enumerate() {
        let res = (|| -> Result<()> {
            let raw = archive.read_raw(id)?;
            let size = if size_ok { format!(" SIZE={}", raw.len()) } else { String::new() };
            c.cmd(&format!("MAIL FROM:<{}>{size}", t.from), &[250])?;
            for to in &t.to {
                c.cmd(&format!("RCPT TO:<{to}>"), &[250, 251])?;
            }
            c.cmd("DATA", &[354])?;
            let w = c.r.get_mut();
            w.write_all(&smtp_data(&raw))?;
            w.flush()?;
            let (code, text) = c.reply()?;
            if code != 250 {
                bail!("message refused: {code} {text}");
            }
            Ok(())
        })();
        match res {
            Ok(()) => st.sent += 1,
            Err(e) => {
                st.fail(format!("{}: {e:#}", &id[..12.min(id.len())]));
                // Keep the session usable for the next message.
                if c.cmd("RSET", &[250]).is_err() {
                    let (nc, _) = connect_smtp(t)?;
                    c = nc;
                }
            }
        }
        if i % 20 == 0 {
            progress(&st);
        }
    }
    let _ = c.cmd("QUIT", &[221]);
    progress(&st);
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_is_crlf_and_dot_stuffed() {
        assert_eq!(smtp_data(b"A: b\n\n.hidden\nend"), b"A: b\r\n\r\n..hidden\r\nend\r\n.\r\n".to_vec());
        assert_eq!(smtp_data(b"x\r\n.\r\n"), b"x\r\n..\r\n.\r\n".to_vec());
    }
}
