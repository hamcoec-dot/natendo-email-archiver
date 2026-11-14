//! Outlook .pst / .ost import, using Microsoft's `outlook-pst` reader.
//! Items are converted to MIME with the shared MAPI converter; folders keep
//! their Outlook names. Experimental: tested on synthetic data only.

use std::io;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;

use anyhow::Result;
use outlook_pst::ltp::prop_context::PropertyValue;
use outlook_pst::ltp::table_context::TableContext;
use outlook_pst::messaging::attachment::{AnsiAttachment, AttachmentData, UnicodeAttachment};
use outlook_pst::messaging::folder::Folder;
use outlook_pst::messaging::message::{AnsiMessage, Message, UnicodeMessage};
use outlook_pst::messaging::store::{AnsiStore, Store, UnicodeStore};
use outlook_pst::ndb::node_id::NodeId;
use outlook_pst::{AnsiPstFile, UnicodePstFile};

use crate::archive::{ingest, Archive, ImportStats};
use crate::import::mapi::{codepage_charset, filetime_to_unix, MapiAttachment, MapiMessage};

type Item = std::result::Result<(Vec<u8>, String, Vec<String>), String>;

fn text(v: &PropertyValue) -> Option<String> {
    let s = match v {
        PropertyValue::String8(s) => s.buffer().iter().map(|&b| b as char).collect::<String>(),
        PropertyValue::Unicode(s) => String::from_utf16_lossy(s.buffer()),
        _ => return None,
    };
    let s = s.trim_end_matches('\0').to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Subjects may start with a 0x01 marker and a prefix length.
fn subject(v: &PropertyValue) -> Option<String> {
    let s = text(v)?;
    let mut c = s.chars();
    if c.next() == Some('\u{1}') {
        c.next();
        return Some(c.collect());
    }
    Some(s)
}

fn int(v: &PropertyValue) -> Option<i64> {
    match v {
        PropertyValue::Integer32(i) => Some(*i as i64),
        PropertyValue::Integer16(i) => Some(*i as i64),
        PropertyValue::Integer64(i) => Some(*i),
        PropertyValue::Time(t) => Some(*t),
        _ => None,
    }
}

fn binary(v: &PropertyValue) -> Option<Vec<u8>> {
    match v {
        PropertyValue::Binary(b) => Some(b.buffer().to_vec()),
        _ => None,
    }
}

/// Read named columns of each row of a table.
fn table_rows(t: &dyn TableContext, ids: &[u16]) -> Vec<(u32, Vec<Option<PropertyValue>>)> {
    let ctx = t.context();
    let mut out = Vec::new();
    for row in t.rows_matrix() {
        let Ok(cols) = row.columns(ctx) else { continue };
        let mut vals = vec![None; ids.len()];
        for (col, value) in ctx.columns().iter().zip(cols) {
            if let Some(pos) = ids.iter().position(|&i| i == col.prop_id()) {
                if let Some(value) = value {
                    vals[pos] = t.read_column(&value, col.prop_type()).ok();
                }
            }
        }
        out.push((u32::from(row.id()), vals));
    }
    out
}

fn convert(m: &dyn Message) -> MapiMessage {
    let p = m.properties();
    let g = |id: u16| p.get(id);
    let mut out = MapiMessage {
        transport_headers: g(0x007D).and_then(text),
        subject: g(0x0037).and_then(subject).unwrap_or_default(),
        sender_name: g(0x0C1A).and_then(text).or_else(|| g(0x0042).and_then(text)).unwrap_or_default(),
        sender_email: g(0x5D01).and_then(text).or_else(|| g(0x0C1F).and_then(text)).or_else(|| g(0x0065).and_then(text)).unwrap_or_default(),
        date: g(0x0E06).and_then(int).or_else(|| g(0x0039).and_then(int)).map(filetime_to_unix),
        message_id: g(0x1035).and_then(text),
        in_reply_to: g(0x1042).and_then(text),
        body: g(0x1000).and_then(text),
        ..Default::default()
    };
    if !out.sender_email.contains('@') {
        out.sender_email.clear();
    }
    let cp = g(0x3FDE).and_then(int).unwrap_or(65001) as i32;
    if let Some(h) = g(0x1013).and_then(binary) {
        out.html = Some((h, codepage_charset(cp).to_string()));
    } else if let Some(h) = g(0x1013).and_then(text) {
        out.html = Some((h.into_bytes(), "utf-8".into()));
    }
    if let Some(rt) = m.recipient_table() {
        for (_, v) in table_rows(rt.as_ref(), &[0x0C15, 0x3001, 0x39FE, 0x3003]) {
            let kind = v[0].as_ref().and_then(int).unwrap_or(1) as i32;
            let name = v[1].as_ref().and_then(text).unwrap_or_default();
            let email = v[2].as_ref().and_then(text).or_else(|| v[3].as_ref().and_then(text)).filter(|e| e.contains('@')).unwrap_or_default();
            out.recipients.push((kind, name, email));
        }
    }
    out
}

fn attachment(props: &outlook_pst::messaging::attachment::AttachmentProperties, data: Option<&AttachmentData>) -> MapiAttachment {
    let g = |id: u16| props.get(id);
    let mut a = MapiAttachment {
        name: g(0x3707).and_then(text).or_else(|| g(0x3704).and_then(text)).or_else(|| g(0x3001).and_then(text)).unwrap_or_default(),
        mime: g(0x370E).and_then(text).unwrap_or_default(),
        content_id: g(0x3712).and_then(text),
        ..Default::default()
    };
    match data {
        Some(AttachmentData::Binary(b)) => a.data = b.buffer().to_vec(),
        Some(AttachmentData::Message(m)) => a.embedded = Some(Box::new(convert(m.as_ref()))),
        None => {}
    }
    a
}

macro_rules! walker {
    ($name:ident, $File:ty, $Store:ty, $Msg:ty, $Att:ty) => {
        fn $name(path: &Path, src: &str, tx: &mpsc::SyncSender<Item>) -> io::Result<()> {
            let store: Rc<$Store> = <$Store>::read(Rc::new(<$File>::open(path)?))?;
            let ipm = store.properties().ipm_sub_tree_entry_id()?;
            let root = store.open_folder(&ipm)?;
            let mut stack: Vec<(Rc<dyn Folder>, String)> = vec![(root, String::new())];
            while let Some((folder, fpath)) = stack.pop() {
                if let Some(ht) = folder.hierarchy_table() {
                    for row in ht.rows_matrix() {
                        let Ok(eid) = store.properties().make_entry_id(NodeId::from(u32::from(row.id()))) else { continue };
                        if let Ok(sub) = store.open_folder(&eid) {
                            let name = sub.properties().display_name().map(|s| s.to_string()).unwrap_or_default();
                            let p = if fpath.is_empty() { name } else { format!("{fpath}/{name}") };
                            stack.push((sub, p));
                        }
                    }
                }
                let Some(ct) = folder.contents_table() else { continue };
                for row in ct.rows_matrix() {
                    let item = (|| -> io::Result<Vec<u8>> {
                        let eid = store.properties().make_entry_id(NodeId::from(u32::from(row.id())))?;
                        let msg = <$Msg>::read(store.clone(), &eid, None)?;
                        let mut m = convert(msg.as_ref());
                        if let Some(at) = msg.attachment_table() {
                            for (id, _) in table_rows(at.as_ref(), &[]) {
                                if let Ok(a) = <$Att>::read(msg.clone(), NodeId::from(id), None) {
                                    use outlook_pst::messaging::attachment::Attachment;
                                    m.attachments.push(attachment(a.properties(), a.data()));
                                }
                            }
                        }
                        Ok(m.to_eml())
                    })();
                    let sent = match item {
                        Ok(raw) => tx.send(Ok((raw, src.to_string(), vec![fpath.clone()]))),
                        Err(e) => tx.send(Err(format!("{}: item in {fpath}: {e}", path.display()))),
                    };
                    if sent.is_err() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        }
    };
}

walker!(walk_unicode, UnicodePstFile, UnicodeStore, UnicodeMessage, UnicodeAttachment);
walker!(walk_ansi, AnsiPstFile, AnsiStore, AnsiMessage, AnsiAttachment);

pub fn import(archive: &mut Archive, path: &Path, src: &str, progress: impl FnMut(&ImportStats)) -> Result<ImportStats> {
    let mut stats = ImportStats::default();
    let (tx, rx) = mpsc::sync_channel::<Item>(256);
    let p = path.to_path_buf();
    let s = src.to_string();
    // The PST reader is single-threaded (Rc-based); it runs on its own thread
    // and streams converted messages to the parallel ingest pipeline.
    let reader = std::thread::spawn(move || {
        let r = if UnicodePstFile::open(&p).is_ok() { walk_unicode(&p, &s, &tx) } else { walk_ansi(&p, &s, &tx) };
        if let Err(e) = r {
            let _ = tx.send(Err(format!("{}: cannot read as PST/OST: {e}", p.display())));
        }
    });
    ingest(archive, rx.into_iter(), &mut stats, progress)?;
    let _ = reader.join();
    archive.record_import("pst", &path.display().to_string(), &stats)?;
    Ok(stats)
}
