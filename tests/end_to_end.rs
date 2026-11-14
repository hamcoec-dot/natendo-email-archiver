//! End-to-end tests: import, search features, dedupe, multi-machine sync,
//! export round trip, verify.

use std::fs;
use std::path::Path;

use nea::archive::{Archive, OpenOptions};
use nea::import::{eml, mbox};
use nea::query::Sort;

fn writer(root: &Path, cache: &Path) -> Archive {
    Archive::open(root, &OpenOptions { cache_dir: Some(cache.to_path_buf()), writer: true, create: true, ..Default::default() }).unwrap()
}

fn reader(root: &Path, cache: &Path) -> Archive {
    Archive::open(root, &OpenOptions { cache_dir: Some(cache.to_path_buf()), ..Default::default() }).unwrap()
}

fn msg(from: &str, to: &str, subject: &str, date: &str, body: &str) -> Vec<u8> {
    format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: {date}\r\nMessage-ID: <{}@test>\r\n\r\n{body}\r\n", subject.replace(' ', "."))
        .into_bytes()
}

fn with_attachment(subject: &str, name: &str, ct: &str, data_b64: &str) -> Vec<u8> {
    format!(
        "From: Lease Office <office@landlord-co.com>\r\nTo: alex@example.com\r\nSubject: {subject}\r\nDate: Tue, 05 Mar 2019 10:00:00 +0000\r\nMessage-ID: <{subject}@att>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"XX\"\r\n\r\n--XX\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n--XX\r\nContent-Type: {ct}; name=\"{name}\"\r\nContent-Disposition: attachment; filename=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{data_b64}\r\n--XX--\r\n"
    )
    .into_bytes()
}

fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn ids(a: &Archive, q: &str) -> usize {
    a.index.search(q, None, 0, 100).unwrap().total
}

fn sample(a: &mut Archive) {
    let src = "test".to_string();
    let f = |s: &str| vec![s.to_string()];
    a.add(&msg("Dana Smith <dana@acme.com>", "alex@example.com", "Invoice 4471 for March", "Tue, 05 Mar 2019 10:00:00 +0000", "Please find invoice #4471 attached. Total 120 USD."), &src, &f("INBOX")).unwrap();
    a.add(&msg("Bob Jones <bob@example.org>", "alex@example.com", "Dinner plans", "Fri, 12 Jul 2021 18:00:00 +0000", "Are we still on for dinner? The landlord agreed to fix the heater."), &src, &f("INBOX")).unwrap();
    a.add(&msg("News <news@shop.example>", "alex@example.com", "Weekly newsletter", "Mon, 01 Feb 2021 08:00:00 +0000", "Big invoice savings this week! Receipts and more."), &src, &f("Promotions")).unwrap();
    a.add(&msg("Priya Patel <priya@acme.com>", "alex@example.com, bob@example.org", "Receipt for your order", "Wed, 10 Nov 2022 09:00:00 +0000", "Your receipt. Code user_id=99 and C++ notes."), &src, &f("Receipts/2022")).unwrap();
    a.commit().unwrap();
}

#[test]
fn search_features() {
    let d = tempfile::tempdir().unwrap();
    let mut a = writer(&d.path().join("arch"), &d.path().join("cache"));
    sample(&mut a);
    assert_eq!(ids(&a, ""), 4);
    assert_eq!(ids(&a, "invoice"), 2);
    assert_eq!(ids(&a, "invoice -newsletter"), 1);
    assert_eq!(ids(&a, "invoice OR receipt"), 3);
    // stemming: "receipts" finds "receipt"
    assert_eq!(ids(&a, "receipts"), 2);
    // typo tolerance
    assert_eq!(ids(&a, "invoce"), 2);
    assert_eq!(ids(&a, "landlrod"), 1);
    // phrase + phrase prefix
    assert_eq!(ids(&a, "\"fix the heater\""), 1);
    assert_eq!(ids(&a, "\"dinner pl*\""), 1);
    // fields
    assert_eq!(ids(&a, "from:dana"), 1);
    assert_eq!(ids(&a, "from:dan"), 1, "prefix on address");
    assert_eq!(ids(&a, "from:dana@acme.com"), 1);
    assert_eq!(ids(&a, "to:bob"), 1);
    assert_eq!(ids(&a, "subject:dinner"), 1);
    assert_eq!(ids(&a, "domain:acme.com"), 2);
    assert_eq!(ids(&a, "sender:priya@acme.com"), 1);
    // dates
    assert_eq!(ids(&a, "year:2021"), 2);
    assert_eq!(ids(&a, "after:2021 before:2022"), 2);
    assert_eq!(ids(&a, "month:2019-03"), 1);
    assert_eq!(ids(&a, "date:\"march 2019\""), 1);
    // folders
    assert_eq!(ids(&a, "folder:receipts"), 1);
    assert_eq!(ids(&a, "infolder:test/Receipts/2022"), 1);
    assert_eq!(ids(&a, "in:promotions"), 1);
    // special characters
    assert_eq!(ids(&a, "#4471"), 1);
    assert_eq!(ids(&a, "user_id=99"), 1);
    assert_eq!(ids(&a, "C++"), 1);
    // stray punctuation and bad dates are ignored, not fatal
    assert_eq!(ids(&a, "invoice - &"), 2);
    assert_eq!(ids(&a, "invoice after:someday"), 2);
    // recall mode
    assert_eq!(ids(&a, "~heater ~savings"), 2);
    // garbage never errors
    for q in ["\"", "((", "from:", "-", "OR", ":::", "*", "?"] {
        a.index.search(q, None, 0, 10).unwrap();
    }
    // ranking: subject match outranks body-only match
    let r = a.index.search("invoice", Some(Sort::Relevance), 0, 10).unwrap();
    assert_eq!(r.hits[0].subject, "Invoice 4471 for March");
    assert!(r.hits[0].snippet.contains("<b>"), "snippet highlights: {}", r.hits[0].snippet);
    // sorting
    let r = a.index.search("", Some(Sort::Oldest), 0, 10).unwrap();
    assert_eq!(r.hits[0].subject, "Invoice 4471 for March");
    // facets
    let r = a.index.search("", None, 0, 10).unwrap();
    assert!(r.facets.sender.iter().any(|b| b.label == "acme.com" && b.count == 2));
    assert!(r.facets.date.iter().any(|b| b.label == "2021" && b.count == 2));
    // facets drill down when one bucket remains
    let r = a.index.search("domain:acme.com", None, 0, 10).unwrap();
    assert!(r.facets.sender.iter().any(|b| b.label == "dana@acme.com"), "{:?}", r.facets.sender);
    // facet filters round-trip
    for b in r.facets.sender.iter().chain(&r.facets.date).chain(&r.facets.folder) {
        assert_eq!(ids(&a, &format!("domain:acme.com {}", b.filter)), b.count as usize, "facet filter {}", b.filter);
    }
}

#[test]
fn attachments_are_searchable() {
    let d = tempfile::tempdir().unwrap();
    let mut a = writer(&d.path().join("arch"), &d.path().join("cache"));
    // DOCX built on the fly
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        use std::io::Write;
        let mut z = zip::ZipWriter::new(&mut buf);
        z.start_file::<_, ()>("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"<w:document><w:body><w:p><w:r><w:t>Lease renewal terms for unit 4B</w:t></w:r></w:p></w:body></w:document>").unwrap();
        z.finish().unwrap();
    }
    a.add(&with_attachment("Lease docs", "lease.docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document", &b64(buf.get_ref())), "t", &["INBOX".into()]).unwrap();
    a.add(&with_attachment("Notes", "notes.txt", "text/plain", &b64(b"the quarterly zebra migration plan")), "t", &["INBOX".into()]).unwrap();
    a.commit().unwrap();
    assert_eq!(ids(&a, "zebra"), 1);
    assert_eq!(ids(&a, "att:\"lease renewal\""), 1);
    assert_eq!(ids(&a, "has:attachment"), 2);
    assert_eq!(ids(&a, "type:docx"), 1);
    assert_eq!(ids(&a, "filename:*.txt"), 1);
    assert_eq!(ids(&a, "filename:lease"), 1);
    let r = a.index.search("zebra", None, 0, 10).unwrap();
    assert!(r.hits[0].att_snippet.contains("<b>zebra</b>"), "{}", r.hits[0].att_snippet);
    let r = a.index.search("", None, 0, 10).unwrap();
    assert!(r.facets.r#type.iter().any(|b| b.label == "DOCX"), "{:?}", r.facets.r#type);
}

#[test]
fn dedupe_and_locations() {
    let d = tempfile::tempdir().unwrap();
    let mut a = writer(&d.path().join("arch"), &d.path().join("cache"));
    let m = msg("a@b.com", "c@d.com", "Same message", "Tue, 05 Mar 2019 10:00:00 +0000", "hello");
    use nea::archive::AddOutcome::*;
    assert_eq!(a.add(&m, "gmail", &["INBOX".into()]).unwrap(), New);
    assert_eq!(a.add(&m, "gmail", &["INBOX".into()]).unwrap(), Duplicate);
    assert_eq!(a.add(&m, "mailstore", &["Old/INBOX".into()]).unwrap(), NewLocation);
    a.commit().unwrap();
    assert_eq!(ids(&a, ""), 1);
    assert_eq!(ids(&a, "source:mailstore"), 1);
    assert_eq!(ids(&a, "source:gmail"), 1);
    assert_eq!(a.catalog.locations(&nea::archive::sha256_hex(&m)).unwrap().len(), 2);
}

#[test]
fn second_machine_catches_up_from_journal() {
    // Simulates a NAS: one archive folder, two machines with their own caches.
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("nas-archive");
    {
        let mut a = writer(&root, &d.path().join("cache-pc1"));
        sample(&mut a);
    }
    let b = reader(&root, &d.path().join("cache-pc2"));
    assert_eq!(ids(&b, ""), 4);
    assert_eq!(ids(&b, "folder:receipts"), 1);
    drop(b);
    // PC1 adds more; PC2 picks it up on next open.
    {
        let mut a = writer(&root, &d.path().join("cache-pc1"));
        a.add(&msg("z@z.com", "y@y.com", "Later message", "Tue, 05 Mar 2024 10:00:00 +0000", "fresh"), "test", &["INBOX".into()]).unwrap();
        a.add(&msg("Dana Smith <dana@acme.com>", "alex@example.com", "Invoice 4471 for March", "Tue, 05 Mar 2019 10:00:00 +0000", "Please find invoice #4471 attached. Total 120 USD."), "other", &["Archive".into()]).unwrap();
    }
    let mut b = reader(&root, &d.path().join("cache-pc2"));
    assert_eq!(ids(&b, ""), 5);
    assert_eq!(ids(&b, "fresh"), 1);
    assert_eq!(ids(&b, "source:other"), 1, "new location re-indexed");
    // idempotent
    assert_eq!(b.try_sync(), 0);
}

#[test]
fn writer_lock_is_exclusive() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("arch");
    let _a = writer(&root, &d.path().join("c1"));
    let r = Archive::open(&root, &OpenOptions { cache_dir: Some(d.path().join("c2")), writer: true, ..Default::default() });
    assert!(r.is_err());
    // readers are fine
    reader(&root, &d.path().join("c3"));
}

#[test]
fn eml_mailstore_and_mbox_import_and_export_roundtrip() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("mailstore-export");
    fs::create_dir_all(src.join("alex@example.com/INBOX/Receipts")).unwrap();
    fs::create_dir_all(src.join("alex@example.com/Sent")).unwrap();
    fs::write(src.join("alex@example.com/INBOX/Receipts/1.eml"), msg("a@b.com", "n@g.com", "Receipt one", "Tue, 05 Mar 2019 10:00:00 +0000", "r1")).unwrap();
    fs::write(src.join("alex@example.com/Sent/2.eml"), msg("n@g.com", "a@b.com", "Sent two", "Tue, 05 Mar 2020 10:00:00 +0000", "s2")).unwrap();
    fs::write(src.join("alex@example.com/Sent/3.msg"), b"not a real msg file").unwrap();
    let root = d.path().join("arch");
    let mut a = writer(&root, &d.path().join("cache"));
    let st = eml::import(&mut a, &src, "mailstore", eml::Flavor::MailStore, |_| {}).unwrap();
    assert_eq!(st.added, 2);
    assert_eq!(st.failed, 1, "broken .msg reported, import continues");
    assert_eq!(ids(&a, "infolder:alex@example.com/INBOX/Receipts"), 1);
    assert_eq!(ids(&a, "source:alex@example.com"), 2);

    // Export to mbox, import into a new archive, same content hashes.
    let all = a.index.all_ids("").unwrap();
    let mbox_path = d.path().join("out/all.mbox");
    let ex = nea::export::to_mbox(&a, &all, &mbox_path).unwrap();
    assert_eq!(ex.written, 2);
    let mut b = writer(&d.path().join("arch2"), &d.path().join("cache2"));
    let st = mbox::import(&mut b, &mbox_path, "mbox", |_| {}).unwrap();
    assert_eq!(st.added, 2);
    let mut x = b.index.all_ids("").unwrap();
    let mut y = all.clone();
    x.sort();
    y.sort();
    assert_eq!(x, y, "byte-for-byte round trip");

    // EML export mirrors folders
    let out = d.path().join("out/eml");
    let ex = nea::export::to_eml_dir(&a, &all, &out).unwrap();
    assert_eq!(ex.written, 2);
    assert!(out.join("alex@example.com/INBOX/Receipts").read_dir().unwrap().count() == 1);

    // Verify detects corruption
    let r = a.verify(|_| {}).unwrap();
    assert_eq!(r.ok, 2);
    let (pack, off, _) = a.locate(&all[0]).unwrap().unwrap();
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = fs::OpenOptions::new().write(true).open(&pack).unwrap();
        f.seek(SeekFrom::Start(off)).unwrap();
        f.write_all(b"XX").unwrap();
    }
    let r = a.verify(|_| {}).unwrap();
    assert_eq!(r.corrupt.len(), 1);
}

#[test]
fn takeout_labels_become_folders() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("All mail.mbox");
    let mut data = Vec::new();
    data.extend_from_slice(b"From 123@xxx Mon Jan  1 00:00:00 2024\r\nX-Gmail-Labels: Inbox,Travel/2023,Important\r\nFrom: air@airline.travel\r\nSubject: Your flight itinerary\r\nDate: Mon, 01 Jan 2024 00:00:00 +0000\r\n\r\nGate B12\r\n\r\n");
    data.extend_from_slice(b"From 456@xxx Mon Jan  1 00:00:00 2024\r\nX-Gmail-Labels: Archived\r\nFrom: x@y.com\r\nSubject: Other\r\nDate: Mon, 01 Jan 2024 00:00:00 +0000\r\n\r\nhello\r\n");
    fs::write(&p, data).unwrap();
    let mut a = writer(&d.path().join("arch"), &d.path().join("cache"));
    let st = mbox::import(&mut a, &p, "gmail-takeout", |_| {}).unwrap();
    assert_eq!(st.added, 2);
    assert_eq!(ids(&a, "label:travel"), 1);
    assert_eq!(ids(&a, "label:important itinerary"), 1);
}

#[test]
fn rebuild_restores_index() {
    let d = tempfile::tempdir().unwrap();
    let mut a = writer(&d.path().join("arch"), &d.path().join("cache"));
    sample(&mut a);
    a.rebuild_index(|_, _| {}).unwrap();
    assert_eq!(ids(&a, ""), 4);
    assert_eq!(ids(&a, "invoice"), 2);
}

#[test]
fn bulk_synthetic_import() {
    let d = tempfile::tempdir().unwrap();
    let mut data = Vec::new();
    for i in 0..500 {
        data.extend_from_slice(b"From x@y Mon Jan  1 00:00:00 2024\n");
        data.extend_from_slice(&nea::synth::message(1, i));
        data.extend_from_slice(b"\n");
    }
    let p = d.path().join("bulk.mbox");
    fs::write(&p, data).unwrap();
    let mut a = writer(&d.path().join("arch"), &d.path().join("cache"));
    let st = mbox::import(&mut a, &p, "bulk", |_| {}).unwrap();
    assert_eq!(st.added + st.duplicates, 500);
    assert_eq!(st.failed, 0);
    assert_eq!(ids(&a, ""), st.added as usize);
    // importing again adds nothing
    let st2 = mbox::import(&mut a, &p, "bulk", |_| {}).unwrap();
    assert_eq!(st2.added, 0);
    drop(a);
    // another computer opening the same archive builds an identical index
    let b = reader(&d.path().join("arch"), &d.path().join("cache-other"));
    assert_eq!(ids(&b, ""), st.added as usize);
    assert_eq!(ids(&b, "has:attachment"), {
        let a = reader(&d.path().join("arch"), &d.path().join("cache"));
        ids(&a, "has:attachment")
    });
}

#[test]
fn empty_pst_imports_cleanly() {
    let d = tempfile::tempdir().unwrap();
    let mut a = writer(&d.path().join("arch"), &d.path().join("cache"));
    let st = nea::import::pst::import(&mut a, Path::new("tests/fixtures/Empty.pst"), "outlook", |_| {}).unwrap();
    assert_eq!((st.added, st.failed), (0, 0), "{:?}", st.errors);
    let st = nea::import::pst::import(&mut a, Path::new("Cargo.toml"), "outlook", |_| {}).unwrap();
    assert_eq!(st.failed, 1, "non-PST reported, not a crash");
}
