//! Write Outlook .msg files (MS-OXMSG) from archived MIME messages, so
//! exported mail can be dragged into Outlook or opened by double-click.
//! The original headers travel in PR_TRANSPORT_MESSAGE_HEADERS.

use std::io::{Cursor, Write};

use anyhow::Result;

use crate::parse;

const PT_LONG: u16 = 0x0003;
const PT_BOOLEAN: u16 = 0x000B;
const PT_SYSTIME: u16 = 0x0040;
const PT_UNICODE: u16 = 0x001F;
const PT_BINARY: u16 = 0x0102;

enum Val {
    Long(u32),
    Bool(bool),
    Time(i64),
    Str(String),
    Bin(Vec<u8>),
}

struct Props(Vec<(u16, Val)>);

impl Props {
    fn new() -> Self {
        Props(Vec::new())
    }
    fn s(&mut self, id: u16, v: impl Into<String>) -> &mut Self {
        let v = v.into();
        if !v.is_empty() {
            self.0.push((id, Val::Str(v)));
        }
        self
    }
    fn l(&mut self, id: u16, v: u32) -> &mut Self {
        self.0.push((id, Val::Long(v)));
        self
    }
    fn t(&mut self, id: u16, unix: i64) -> &mut Self {
        if unix > 0 {
            self.0.push((id, Val::Time(unix)));
        }
        self
    }
    fn b(&mut self, id: u16, v: Vec<u8>) -> &mut Self {
        self.0.push((id, Val::Bin(v)));
        self
    }
    fn flag(&mut self, id: u16, v: bool) -> &mut Self {
        self.0.push((id, Val::Bool(v)));
        self
    }

    /// Write the property stream (with `header` bytes before the entries)
    /// and one substream per variable-length property into `dir`.
    fn write(&self, cf: &mut cfb::CompoundFile<Cursor<Vec<u8>>>, dir: &str, header: &[u8]) -> Result<()> {
        let mut table = header.to_vec();
        for (id, v) in &self.0 {
            let (ty, value): (u16, [u8; 8]) = match v {
                Val::Long(n) => (PT_LONG, pad(&n.to_le_bytes())),
                Val::Bool(b) => (PT_BOOLEAN, pad(&[*b as u8])),
                Val::Time(t) => (PT_SYSTIME, ((*t + 11_644_473_600) as u64 * 10_000_000).to_le_bytes()),
                Val::Str(s) => {
                    let data = utf16(s);
                    stream(cf, &format!("{dir}/__substg1.0_{id:04X}{PT_UNICODE:04X}"), &data)?;
                    (PT_UNICODE, pad(&((data.len() + 2) as u32).to_le_bytes()))
                }
                Val::Bin(b) => {
                    stream(cf, &format!("{dir}/__substg1.0_{id:04X}{PT_BINARY:04X}"), b)?;
                    (PT_BINARY, pad(&(b.len() as u32).to_le_bytes()))
                }
            };
            table.extend_from_slice(&(((*id as u32) << 16) | ty as u32).to_le_bytes());
            table.extend_from_slice(&6u32.to_le_bytes()); // readable | writable
            table.extend_from_slice(&value);
        }
        stream(cf, &format!("{dir}/__properties_version1.0"), &table)
    }
}

fn pad(b: &[u8]) -> [u8; 8] {
    let mut v = [0u8; 8];
    v[..b.len()].copy_from_slice(b);
    v
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

fn stream(cf: &mut cfb::CompoundFile<Cursor<Vec<u8>>>, path: &str, data: &[u8]) -> Result<()> {
    cf.create_stream(path)?.write_all(data)?;
    Ok(())
}

fn header_block(raw: &[u8]) -> String {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|p| p + 2))
        .unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

/// Convert one MIME message to .msg bytes.
pub fn eml_to_msg(raw: &[u8]) -> Result<Vec<u8>> {
    let p = parse::parse(raw);
    let mut cf = cfb::CompoundFile::create(Cursor::new(Vec::new()))?;

    // Named-property mapping storage: Outlook expects it, even empty.
    cf.create_storage("/__nameid_version1.0")?;
    for s in ["00020102", "00030102", "00040102"] {
        stream(&mut cf, &format!("/__nameid_version1.0/__substg1.0_{s}"), &[])?;
    }

    let join = |v: &[parse::Addr]| v.iter().map(|a| if a.name.is_empty() { a.email.clone() } else { a.name.clone() }).collect::<Vec<_>>().join("; ");
    let from = p.from.first().cloned().unwrap_or_default();
    let mut root = Props::new();
    root.s(0x001A, "IPM.Note")
        .s(0x0037, p.subject.clone())
        .s(0x0E1D, p.subject.clone())
        .s(0x0C1A, if from.name.is_empty() { from.email.clone() } else { from.name.clone() })
        .s(0x0C1E, "SMTP")
        .s(0x0C1F, from.email.clone())
        .s(0x5D01, from.email.clone())
        .s(0x0042, if from.name.is_empty() { from.email.clone() } else { from.name.clone() })
        .s(0x0064, "SMTP")
        .s(0x0065, from.email.clone())
        .s(0x5D02, from.email.clone())
        .s(0x0E04, join(&p.to))
        .s(0x0E03, join(&p.cc))
        .s(0x0E02, join(&p.bcc))
        .s(0x1000, p.body_text.clone())
        .s(0x007D, header_block(raw))
        .s(0x1035, if p.message_id.is_empty() { String::new() } else { format!("<{}>", p.message_id) })
        .s(0x1042, p.in_reply_to.first().map(|m| format!("<{m}>")).unwrap_or_default())
        .t(0x0039, p.date)
        .t(0x0E06, p.date)
        .l(0x0E07, 1) // MSGFLAG_READ
        .l(0x340D, 0x0004_0000) // STORE_UNICODE_OK
        .l(0x0017, match p.priority {
            1 => 2,
            5 => 0,
            _ => 1,
        })
        .flag(0x0E1B, p.attachments.iter().any(|a| !a.inline || a.content_id.is_none()));
    if let Some(h) = &p.body_html {
        root.b(0x1013, h.as_bytes().to_vec()).l(0x3FDE, 65001);
    }

    // Recipients.
    let recips: Vec<(u32, &parse::Addr)> =
        p.to.iter().map(|a| (1, a)).chain(p.cc.iter().map(|a| (2, a))).chain(p.bcc.iter().map(|a| (3, a))).collect();
    for (n, (kind, a)) in recips.iter().enumerate() {
        let dir = format!("/__recip_version1.0_#{n:08X}");
        cf.create_storage(&dir)?;
        let mut rp = Props::new();
        rp.l(0x0C15, *kind)
            .s(0x3001, if a.name.is_empty() { a.email.clone() } else { a.name.clone() })
            .s(0x3002, "SMTP")
            .s(0x3003, a.email.clone())
            .s(0x39FE, a.email.clone())
            .l(0x0FFE, 6) // MAPI_MAILUSER
            .l(0x3900, 0)
            .l(0x3000, n as u32);
        rp.write(&mut cf, &dir, &[0u8; 8])?;
    }

    // Attachments.
    let mut n_att = 0u32;
    for a in &p.attachments {
        let Some((name, mime, data)) = parse::attachment_bytes(raw, a.index) else { continue };
        let dir = format!("/__attach_version1.0_#{n_att:08X}");
        cf.create_storage(&dir)?;
        let mut ap = Props::new();
        ap.s(0x3707, name.clone())
            .s(0x3704, name.clone())
            .s(0x3001, name.clone())
            .s(0x370E, mime)
            .s(0x3703, std::path::Path::new(&name).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default())
            .s(0x3712, a.content_id.clone().unwrap_or_default())
            .l(0x3705, 1) // ATTACH_BY_VALUE
            .l(0x0FFE, 7) // MAPI_ATTACH
            .l(0x0E21, n_att)
            .l(0x0E20, data.len() as u32)
            .b(0x3701, data);
        if a.inline && a.content_id.is_some() {
            ap.flag(0x7FFE, true); // hidden (inline image)
        }
        ap.write(&mut cf, &dir, &[0u8; 8])?;
        n_att += 1;
    }

    // Root header: 8 reserved, next recipient id, next attachment id,
    // recipient count, attachment count, 8 reserved.
    let mut header = vec![0u8; 8];
    for v in [recips.len() as u32, n_att, recips.len() as u32, n_att] {
        header.extend_from_slice(&v.to_le_bytes());
    }
    header.extend_from_slice(&[0u8; 8]);
    root.write(&mut cf, "", &header)?;
    cf.flush()?;
    Ok(cf.into_inner().into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_the_msg_reader() {
        let raw = b"From: Dana Smith <dana@acme.com>\r\nTo: Alex <alex@example.com>, b@example.com\r\nCc: c@example.com\r\nSubject: Lease renewal \xe2\x9c\x93\r\nDate: Tue, 05 Mar 2019 10:00:00 +0000\r\nMessage-ID: <lease-1@acme.com>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"B\"\r\n\r\n--B\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nPlease sign the renewal.\r\n--B\r\nContent-Type: text/plain; name=\"terms.txt\"\r\nContent-Disposition: attachment; filename=\"terms.txt\"\r\n\r\nTwelve months.\r\n--B--\r\n";
        let msg = eml_to_msg(raw).unwrap();
        assert_eq!(&msg[..8], &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1], "compound file signature");
        let back = crate::import::msg::msg_to_eml_bytes(&msg).unwrap();
        let p = parse::parse(&back);
        assert_eq!(p.subject, "Lease renewal ✓");
        assert_eq!(p.from[0].email, "dana@acme.com");
        assert_eq!(p.to.len(), 2);
        assert_eq!(p.cc[0].email, "c@example.com");
        assert_eq!(p.date, 1551780000);
        assert_eq!(p.message_id, "lease-1@acme.com");
        assert!(p.body_text.contains("Please sign the renewal."));
        assert_eq!(p.attachments.len(), 1);
        assert_eq!(p.attachments[0].name, "terms.txt");
        let (_, _, data) = parse::attachment_bytes(&back, p.attachments[0].index).unwrap();
        assert_eq!(data, b"Twelve months.");
    }
}
