//! Outlook .msg files (MS-OXMSG: a Compound File of MAPI property streams).

use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::import::mapi::{codepage_charset, filetime_to_unix, MapiAttachment, MapiMessage};

#[derive(Debug, Clone)]
enum Prop {
    Str(String),
    Bin(Vec<u8>),
    Int(i64),
}

type Props = HashMap<u16, Prop>;

fn utf16le(b: &[u8]) -> String {
    let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&u).trim_end_matches('\0').to_string()
}

fn read_stream<F: Read + Seek>(cf: &mut cfb::CompoundFile<F>, p: &Path) -> Result<Vec<u8>> {
    let mut s = cf.open_stream(p)?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Read the properties of one storage. `header` is the size of the
/// fixed header of its `__properties_version1.0` stream (32 top-level
/// message, 24 embedded message, 8 recipient/attachment).
fn read_props<F: Read + Seek>(cf: &mut cfb::CompoundFile<F>, dir: &Path, header: usize) -> Result<(Props, Vec<PathBuf>)> {
    let mut props = Props::new();
    let mut children = Vec::new();
    let entries: Vec<(String, PathBuf, bool)> = cf
        .read_storage(dir)?
        .map(|e| (e.name().to_string(), e.path().to_path_buf(), e.is_stream()))
        .collect();
    for (name, path, is_stream) in entries {
        if !is_stream {
            children.push(path);
            continue;
        }
        if let Some(tag) = name.strip_prefix("__substg1.0_") {
            if tag.len() < 8 {
                continue;
            }
            let (Ok(id), Ok(ty)) = (u16::from_str_radix(&tag[..4], 16), u16::from_str_radix(&tag[4..8], 16)) else { continue };
            let data = read_stream(cf, &path)?;
            let v = match ty {
                0x001F => Prop::Str(utf16le(&data)),
                0x001E => Prop::Str(String::from_utf8_lossy(&data).trim_end_matches('\0').to_string()),
                0x0102 => Prop::Bin(data),
                _ => continue,
            };
            props.insert(id, v);
        } else if name == "__properties_version1.0" {
            let data = read_stream(cf, &path)?;
            for e in data.get(header..).unwrap_or(&[]).chunks_exact(16) {
                let ty = u16::from_le_bytes([e[0], e[1]]);
                let id = u16::from_le_bytes([e[2], e[3]]);
                let v = i64::from_le_bytes(e[8..16].try_into().unwrap());
                match ty {
                    0x0003 => {
                        props.insert(id, Prop::Int(v as i32 as i64));
                    }
                    0x0014 | 0x0040 => {
                        props.insert(id, Prop::Int(v));
                    }
                    0x000B => {
                        props.insert(id, Prop::Int((v & 0xFF) as i64));
                    }
                    _ => {}
                }
            }
        }
    }
    Ok((props, children))
}

fn s(p: &Props, id: u16) -> Option<String> {
    match p.get(&id)? {
        Prop::Str(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}
fn i(p: &Props, id: u16) -> Option<i64> {
    match p.get(&id)? {
        Prop::Int(v) => Some(*v),
        _ => None,
    }
}
fn b(p: &Props, id: u16) -> Option<&Vec<u8>> {
    match p.get(&id)? {
        Prop::Bin(v) => Some(v),
        _ => None,
    }
}

fn read_message<F: Read + Seek>(cf: &mut cfb::CompoundFile<F>, dir: &Path, header: usize) -> Result<MapiMessage> {
    let (p, children) = read_props(cf, dir, header)?;
    let mut m = MapiMessage {
        transport_headers: s(&p, 0x007D),
        subject: s(&p, 0x0037).unwrap_or_default(),
        sender_name: s(&p, 0x0C1A).or_else(|| s(&p, 0x0042)).unwrap_or_default(),
        // PR_SENDER_SMTP_ADDRESS, then PR_SENDER_EMAIL_ADDRESS (may be an Exchange DN)
        sender_email: s(&p, 0x5D01).or_else(|| s(&p, 0x0C1F)).or_else(|| s(&p, 0x0065)).unwrap_or_default(),
        date: i(&p, 0x0E06).or_else(|| i(&p, 0x0039)).map(filetime_to_unix),
        message_id: s(&p, 0x1035),
        in_reply_to: s(&p, 0x1042),
        body: s(&p, 0x1000),
        ..Default::default()
    };
    if !m.sender_email.contains('@') {
        m.sender_email.clear();
    }
    let cp = i(&p, 0x3FDE).unwrap_or(65001) as i32;
    if let Some(h) = b(&p, 0x1013) {
        m.html = Some((h.clone(), codepage_charset(cp).to_string()));
    } else if let Some(h) = s(&p, 0x1013) {
        m.html = Some((h.into_bytes(), "utf-8".into()));
    }
    let mut kids = children;
    kids.sort();
    for child in kids {
        let name = child.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.starts_with("__recip_version1.0_") {
            let (rp, _) = read_props(cf, &child, 8)?;
            let email = s(&rp, 0x39FE).or_else(|| s(&rp, 0x3003)).filter(|e| e.contains('@')).unwrap_or_default();
            m.recipients.push((i(&rp, 0x0C15).unwrap_or(1) as i32, s(&rp, 0x3001).unwrap_or_default(), email));
        } else if name.starts_with("__attach_version1.0_") {
            let (ap, _) = read_props(cf, &child, 8)?;
            let mut a = MapiAttachment {
                name: s(&ap, 0x3707).or_else(|| s(&ap, 0x3704)).or_else(|| s(&ap, 0x3001)).unwrap_or_default(),
                mime: s(&ap, 0x370E).unwrap_or_default(),
                content_id: s(&ap, 0x3712),
                data: b(&ap, 0x3701).cloned().unwrap_or_default(),
                embedded: None,
            };
            let nested = child.join("__substg1.0_3701000D");
            if i(&ap, 0x3705) == Some(5) && cf.is_storage(&nested) {
                a.embedded = Some(Box::new(read_message(cf, &nested, 24)?));
            }
            m.attachments.push(a);
        }
    }
    Ok(m)
}

pub fn msg_to_eml_bytes(data: &[u8]) -> Result<Vec<u8>> {
    let mut cf = cfb::CompoundFile::open(std::io::Cursor::new(data)).context("not an Outlook .msg file")?;
    let m = read_message(&mut cf, Path::new("/"), 32)?;
    Ok(m.to_eml())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    /// Build a small .msg the way Outlook lays it out.
    pub fn build_msg() -> Vec<u8> {
        let mut cf = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
        let put = |cf: &mut cfb::CompoundFile<std::io::Cursor<Vec<u8>>>, path: &str, data: &[u8]| {
            cf.create_stream(path).unwrap().write_all(data).unwrap();
        };
        put(&mut cf, "/__substg1.0_0037001F", &utf16("Lease renewal for Unit 4B"));
        put(&mut cf, "/__substg1.0_0C1A001F", &utf16("Lease Office"));
        put(&mut cf, "/__substg1.0_5D01001F", &utf16("office@landlord-co.com"));
        put(&mut cf, "/__substg1.0_1000001F", &utf16("Please sign the attached renewal."));
        put(&mut cf, "/__substg1.0_1035001F", &utf16("<renewal-1@landlord-co.com>"));
        let mut props = vec![0u8; 32];
        // PR_MESSAGE_DELIVERY_TIME (0x0E06, PT_SYSTIME) = 2019-03-05T10:00:00Z
        let ft: i64 = (1_551_780_000 + 11_644_473_600) * 10_000_000;
        props.extend_from_slice(&0x0040u16.to_le_bytes());
        props.extend_from_slice(&0x0E06u16.to_le_bytes());
        props.extend_from_slice(&0u32.to_le_bytes());
        props.extend_from_slice(&ft.to_le_bytes());
        put(&mut cf, "/__properties_version1.0", &props);
        cf.create_storage("/__recip_version1.0_#00000000").unwrap();
        put(&mut cf, "/__recip_version1.0_#00000000/__substg1.0_3001001F", &utf16("Alex Tenant"));
        put(&mut cf, "/__recip_version1.0_#00000000/__substg1.0_39FE001F", &utf16("alex@example.com"));
        let mut rp = vec![0u8; 8];
        rp.extend_from_slice(&0x0003u16.to_le_bytes());
        rp.extend_from_slice(&0x0C15u16.to_le_bytes());
        rp.extend_from_slice(&0u32.to_le_bytes());
        rp.extend_from_slice(&1i64.to_le_bytes());
        put(&mut cf, "/__recip_version1.0_#00000000/__properties_version1.0", &rp);
        cf.create_storage("/__attach_version1.0_#00000000").unwrap();
        put(&mut cf, "/__attach_version1.0_#00000000/__substg1.0_3707001F", &utf16("renewal.txt"));
        put(&mut cf, "/__attach_version1.0_#00000000/__substg1.0_37010102", b"Renewal term: 12 months, rent 1450");
        put(&mut cf, "/__attach_version1.0_#00000000/__properties_version1.0", &vec![0u8; 8]);
        cf.flush().unwrap();
        cf.into_inner().into_inner()
    }

    #[test]
    fn msg_converts() {
        let raw = msg_to_eml_bytes(&build_msg()).unwrap();
        let p = crate::parse::parse(&raw);
        assert_eq!(p.subject, "Lease renewal for Unit 4B");
        assert_eq!(p.from[0].email, "office@landlord-co.com");
        assert_eq!(p.to[0].email, "alex@example.com");
        assert_eq!(p.message_id, "renewal-1@landlord-co.com");
        assert_eq!(p.date, 1_551_780_000);
        assert!(p.body_text.contains("sign the attached"));
        assert_eq!(p.attachments[0].name, "renewal.txt");
        assert!(p.attachments[0].text.as_deref().unwrap().contains("1450"));
    }
}
