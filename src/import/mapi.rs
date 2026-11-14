//! Outlook (MAPI) messages to RFC 5322. Used by the .msg and .pst importers.
//!
//! When Outlook kept the original Internet headers (PR_TRANSPORT_MESSAGE_HEADERS)
//! they are reused verbatim, so Message-ID, Received chains and dates
//! survive. The MIME body is rebuilt from the stored text/HTML/attachments.
//! Output is deterministic, so importing the same item twice dedupes.

use base64::Engine;
use chrono::TimeZone;

#[derive(Debug, Clone, Default)]
pub struct MapiAttachment {
    pub name: String,
    pub mime: String,
    pub content_id: Option<String>,
    pub data: Vec<u8>,
    /// Attached Outlook item (attach method 5).
    pub embedded: Option<Box<MapiMessage>>,
}

#[derive(Debug, Clone, Default)]
pub struct MapiMessage {
    pub transport_headers: Option<String>,
    pub subject: String,
    pub sender_name: String,
    pub sender_email: String,
    /// (MAPI_TO=1 / MAPI_CC=2 / MAPI_BCC=3, display name, email)
    pub recipients: Vec<(i32, String, String)>,
    /// Unix seconds.
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub body: Option<String>,
    /// HTML bytes and their charset label.
    pub html: Option<(Vec<u8>, String)>,
    pub attachments: Vec<MapiAttachment>,
}

/// Windows FILETIME (100 ns ticks since 1601) to Unix seconds.
pub fn filetime_to_unix(ft: i64) -> i64 {
    ft / 10_000_000 - 11_644_473_600
}

pub fn codepage_charset(cp: i32) -> &'static str {
    match cp {
        65001 => "utf-8",
        1252 => "windows-1252",
        1250 => "windows-1250",
        1251 => "windows-1251",
        28591 => "iso-8859-1",
        28592 => "iso-8859-2",
        20127 => "us-ascii",
        932 => "shift_jis",
        936 => "gb2312",
        949 => "euc-kr",
        950 => "big5",
        _ => "utf-8",
    }
}

fn encode_word(s: &str) -> String {
    if s.is_ascii() && !s.contains(['\r', '\n']) {
        return s.to_string();
    }
    format!("=?utf-8?B?{}?=", base64::engine::general_purpose::STANDARD.encode(s.replace(['\r', '\n'], " ")))
}

fn addr(name: &str, email: &str) -> String {
    let name = name.trim();
    let email = email.trim();
    match (name.is_empty() || name == email, email.is_empty()) {
        (true, false) => format!("<{email}>"),
        (false, false) => {
            let n = if name.is_ascii() && !name.contains(['"', ',', '<', '>', '@', ';', ':']) {
                format!("\"{name}\"")
            } else if name.is_ascii() {
                format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                encode_word(name)
            };
            format!("{n} <{email}>")
        }
        (false, true) => format!("\"{}\" <unknown@invalid>", name.replace('"', "")),
        (true, true) => "<unknown@invalid>".into(),
    }
}

fn b64_lines(data: &[u8], out: &mut Vec<u8>) {
    let enc = base64::engine::general_purpose::STANDARD.encode(data);
    for chunk in enc.as_bytes().chunks(76) {
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\r\n");
    }
}

/// Header names the rebuilt MIME body replaces.
fn structural(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "content-type" || n == "content-transfer-encoding" || n == "mime-version" || n.starts_with("content-")
}

fn filter_transport_headers(h: &str) -> String {
    let mut out = String::new();
    let mut keep = false;
    for line in h.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if keep {
                out.push_str(line);
                out.push_str("\r\n");
            }
            continue;
        }
        let name = line.split(':').next().unwrap_or("");
        keep = line.contains(':') && !structural(name);
        if keep {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out
}

fn attachment_name_params(name: &str) -> String {
    if name.is_ascii() && !name.contains(['"', '\\']) {
        format!("filename=\"{name}\"")
    } else {
        let mut enc = String::new();
        for b in name.as_bytes() {
            if b.is_ascii_alphanumeric() || b"-._~".contains(b) {
                enc.push(*b as char);
            } else {
                enc.push_str(&format!("%{b:02X}"));
            }
        }
        format!("filename*=utf-8''{enc}")
    }
}

impl MapiMessage {
    pub fn to_eml(&self) -> Vec<u8> {
        self.to_eml_level(0)
    }

    fn to_eml_level(&self, level: usize) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        match &self.transport_headers {
            Some(h) if h.contains(':') => out.extend_from_slice(filter_transport_headers(h).as_bytes()),
            _ => {
                let mut h = String::new();
                h.push_str(&format!("From: {}\r\n", addr(&self.sender_name, &self.sender_email)));
                for (kind, label) in [(1, "To"), (2, "Cc"), (3, "Bcc")] {
                    let list: Vec<String> = self.recipients.iter().filter(|r| r.0 == kind).map(|r| addr(&r.1, &r.2)).collect();
                    if !list.is_empty() {
                        h.push_str(&format!("{label}: {}\r\n", list.join(",\r\n ")));
                    }
                }
                h.push_str(&format!("Subject: {}\r\n", encode_word(&self.subject)));
                if let Some(d) = self.date.and_then(|d| chrono::Utc.timestamp_opt(d, 0).single()) {
                    h.push_str(&format!("Date: {}\r\n", d.to_rfc2822()));
                }
                if let Some(m) = &self.message_id {
                    let m = m.trim();
                    if !m.is_empty() {
                        h.push_str(&format!("Message-ID: {}\r\n", if m.starts_with('<') { m.to_string() } else { format!("<{m}>") }));
                    }
                }
                if let Some(r) = &self.in_reply_to {
                    if !r.trim().is_empty() {
                        h.push_str(&format!("In-Reply-To: {}\r\n", r.trim()));
                    }
                }
                out.extend_from_slice(h.as_bytes());
            }
        }
        out.extend_from_slice(b"MIME-Version: 1.0\r\n");
        out.extend_from_slice(b"X-Converted-By: Natendo Email Archiver (Outlook item)\r\n");

        let text = self.body.clone().unwrap_or_default();
        let alt_b = format!("=_NEA_alt_{level}");
        let mix_b = format!("=_NEA_mixed_{level}");

        let mut body_part: Vec<u8> = Vec::new();
        let text_part = |buf: &mut Vec<u8>| {
            buf.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n");
            b64_lines(text.as_bytes(), buf);
        };
        match &self.html {
            Some((html, cs)) => {
                body_part.extend_from_slice(format!("Content-Type: multipart/alternative; boundary=\"{alt_b}\"\r\n\r\n--{alt_b}\r\n").as_bytes());
                text_part(&mut body_part);
                body_part.extend_from_slice(format!("--{alt_b}\r\nContent-Type: text/html; charset={cs}\r\nContent-Transfer-Encoding: base64\r\n\r\n").as_bytes());
                b64_lines(html, &mut body_part);
                body_part.extend_from_slice(format!("--{alt_b}--\r\n").as_bytes());
            }
            None => text_part(&mut body_part),
        }

        if self.attachments.is_empty() {
            out.extend_from_slice(&body_part);
            return out;
        }
        out.extend_from_slice(format!("Content-Type: multipart/mixed; boundary=\"{mix_b}\"\r\n\r\n--{mix_b}\r\n").as_bytes());
        out.extend_from_slice(&body_part);
        for a in &self.attachments {
            out.extend_from_slice(format!("--{mix_b}\r\n").as_bytes());
            if let Some(m) = &a.embedded {
                let name = if a.name.is_empty() { format!("{}.eml", m.subject) } else { a.name.clone() };
                out.extend_from_slice(format!("Content-Type: message/rfc822\r\nContent-Disposition: attachment; {}\r\n\r\n", attachment_name_params(&name)).as_bytes());
                out.extend_from_slice(&m.to_eml_level(level + 1));
                out.extend_from_slice(b"\r\n");
                continue;
            }
            let mime = if a.mime.contains('/') { a.mime.clone() } else { "application/octet-stream".into() };
            let disp = if a.content_id.is_some() { "inline" } else { "attachment" };
            out.extend_from_slice(format!("Content-Type: {mime}\r\nContent-Disposition: {disp}; {}\r\nContent-Transfer-Encoding: base64\r\n", attachment_name_params(&a.name)).as_bytes());
            if let Some(cid) = &a.content_id {
                out.extend_from_slice(format!("Content-ID: <{}>\r\n", cid.trim_matches(['<', '>'])).as_bytes());
            }
            out.extend_from_slice(b"\r\n");
            b64_lines(&a.data, &mut out);
        }
        out.extend_from_slice(format!("--{mix_b}--\r\n").as_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MapiMessage {
        MapiMessage {
            subject: "Quarterly résumé".into(),
            sender_name: "Dana Smith".into(),
            sender_email: "dana@acme.com".into(),
            recipients: vec![(1, "Bob".into(), "bob@x.com".into()), (2, "".into(), "cc@x.com".into())],
            date: Some(1_551_780_000),
            message_id: Some("abc@acme.com".into()),
            body: Some("Hello Bob, numbers attached.".into()),
            html: Some((b"<p>Hello <b>Bob</b></p>".to_vec(), "utf-8".into())),
            attachments: vec![MapiAttachment { name: "Q1 numbers.csv".into(), mime: "text/csv".into(), data: b"a,b\n1,2\n".to_vec(), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn converts_and_parses_back() {
        let raw = sample().to_eml();
        let p = crate::parse::parse(&raw);
        assert_eq!(p.subject, "Quarterly résumé");
        assert_eq!(p.from[0].email, "dana@acme.com");
        assert_eq!(p.to[0].email, "bob@x.com");
        assert_eq!(p.cc[0].email, "cc@x.com");
        assert_eq!(p.message_id, "abc@acme.com");
        assert_eq!(p.date, 1_551_780_000);
        assert!(p.body_text.contains("numbers attached"));
        assert!(p.body_html.unwrap().contains("<b>Bob</b>"));
        assert_eq!(p.attachments.len(), 1);
        assert_eq!(p.attachments[0].name, "Q1 numbers.csv");
        assert!(p.attachments[0].text.as_deref().unwrap().contains("1,2"));
        assert_eq!(sample().to_eml(), raw, "deterministic");
    }

    #[test]
    fn transport_headers_are_reused() {
        let mut m = sample();
        m.transport_headers = Some("Received: from x\r\n by y\r\nMessage-ID: <orig@x>\r\nFrom: Orig <orig@x.com>\r\nSubject: Original\r\nContent-Type: text/plain;\r\n charset=us-ascii\r\nDate: Tue, 05 Mar 2019 10:00:00 +0000\r\n\r\n".into());
        let raw = m.to_eml();
        let p = crate::parse::parse(&raw);
        assert_eq!(p.message_id, "orig@x");
        assert_eq!(p.subject, "Original");
        assert_eq!(p.from[0].email, "orig@x.com");
        assert!(p.body_html.is_some(), "rebuilt MIME body, not the stale Content-Type");
        assert!(!String::from_utf8_lossy(&raw).contains("us-ascii"));
    }

    #[test]
    fn embedded_message() {
        let mut outer = sample();
        outer.attachments = vec![MapiAttachment { name: "".into(), embedded: Some(Box::new(sample())), ..Default::default() }];
        let p = crate::parse::parse(&outer.to_eml());
        assert_eq!(p.attachments.len(), 1);
        assert!(p.attachments[0].text.as_deref().unwrap().contains("numbers attached"));
    }

    #[test]
    fn filetime() {
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), 0);
    }
}
