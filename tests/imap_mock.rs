//! IMAP importer against a minimal in-process IMAP server.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use nea::archive::{Archive, OpenOptions};
use nea::import::imap::{import, ImapConfig};

#[derive(Default)]
struct Server {
    /// Behave like Gmail: X-GM-EXT-1, an \All folder, X-GM-LABELS.
    gmail: bool,
    /// uid -> labels (Gmail mode, All Mail only)
    labels: BTreeMap<u32, Vec<String>>,
    /// folder -> (uidvalidity, uid -> raw)
    folders: BTreeMap<String, (u32, BTreeMap<u32, Vec<u8>>)>,
    /// Every UID FETCH of full bodies: (folder, uids)
    body_fetches: Vec<(String, Vec<u32>)>,
}

fn msg(id: &str, subject: &str) -> Vec<u8> {
    format!("From: a@b.com\r\nTo: c@d.com\r\nSubject: {subject}\r\nMessage-ID: <{id}>\r\nDate: Tue, 05 Mar 2019 10:00:00 +0000\r\n\r\nBody of {subject}\r\n").into_bytes()
}

fn header_of(raw: &[u8]) -> &[u8] {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4).unwrap_or(raw.len());
    &raw[..end]
}

fn parse_set(s: &str) -> Vec<u32> {
    s.split(',').filter_map(|x| x.parse().ok()).collect()
}

fn handle(stream: TcpStream, st: Arc<Mutex<Server>>) {
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    w.write_all(b"* OK [CAPABILITY IMAP4rev1] mock ready\r\n").unwrap();
    let mut selected = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let l = line.trim_end();
        let (tag, rest) = l.split_once(' ').unwrap_or((l, ""));
        let upper = rest.to_uppercase();
        let mut out = Vec::new();
        if upper.starts_with("CAPABILITY") {
            let ext = if st.lock().unwrap().gmail { " X-GM-EXT-1" } else { "" };
            out.extend_from_slice(format!("* CAPABILITY IMAP4rev1{ext}\r\n{tag} OK done\r\n").as_bytes());
        } else if upper.starts_with("LOGIN") {
            out.extend_from_slice(format!("{tag} OK LOGIN completed\r\n").as_bytes());
        } else if upper.starts_with("LIST") {
            let s = st.lock().unwrap();
            for name in s.folders.keys() {
                let attr = if s.gmail && name == "[Gmail]/All Mail" { "\\HasNoChildren \\All" } else { "\\HasNoChildren" };
                out.extend_from_slice(format!("* LIST ({attr}) \"/\" \"{name}\"\r\n").as_bytes());
            }
            out.extend_from_slice(b"* LIST (\\Noselect) \"/\" \"Parent\"\r\n");
            out.extend_from_slice(format!("{tag} OK LIST completed\r\n").as_bytes());
        } else if upper.starts_with("EXAMINE") || upper.starts_with("SELECT") {
            selected = rest.split_once(' ').unwrap().1.trim_matches('"').to_string();
            let s = st.lock().unwrap();
            let (uv, msgs) = &s.folders[&selected];
            out.extend_from_slice(
                format!(
                    "* {} EXISTS\r\n* 0 RECENT\r\n* FLAGS (\\Seen)\r\n* OK [UIDVALIDITY {uv}] ok\r\n* OK [UIDNEXT {}] ok\r\n{tag} OK [READ-ONLY] done\r\n",
                    msgs.len(),
                    msgs.keys().last().copied().unwrap_or(0) + 1
                )
                .as_bytes(),
            );
        } else if upper.starts_with("UID SEARCH") {
            // "UID SEARCH UID n:*"
            let start: u32 = rest.rsplit(' ').next().unwrap().trim_end_matches(":*").parse().unwrap();
            let s = st.lock().unwrap();
            let msgs = &s.folders[&selected].1;
            let mut uids: Vec<u32> = msgs.keys().copied().filter(|&u| u >= start).collect();
            if uids.is_empty() {
                // Real servers return the highest UID for n:* past the end.
                uids.extend(msgs.keys().last().copied());
            }
            let list = uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(" ");
            out.extend_from_slice(format!("* SEARCH {list}\r\n{tag} OK SEARCH done\r\n").as_bytes());
        } else if upper.starts_with("UID FETCH") {
            let parts: Vec<&str> = rest.splitn(4, ' ').collect();
            let uids = parse_set(parts[2]);
            let header_only = upper.contains("BODY.PEEK[HEADER]");
            let mut s = st.lock().unwrap();
            if !header_only {
                s.body_fetches.push((selected.clone(), uids.clone()));
            }
            let msgs = &s.folders[&selected].1;
            for (seq, (uid, raw)) in msgs.iter().enumerate() {
                if !uids.contains(uid) {
                    continue;
                }
                let (section, data) = if header_only { ("BODY[HEADER]", header_of(raw)) } else { ("BODY[]", &raw[..]) };
                let labels = if upper.contains("X-GM-LABELS") {
                    let l = s.labels.get(uid).cloned().unwrap_or_default();
                    let l: Vec<String> = l.iter().map(|x| if x.starts_with('\\') { x.clone() } else { format!("\"{x}\"") }).collect();
                    format!(" X-GM-LABELS ({})", l.join(" "))
                } else {
                    String::new()
                };
                out.extend_from_slice(
                    format!("* {} FETCH (UID {uid}{labels} RFC822.SIZE {} {section} {{{}}}\r\n", seq + 1, raw.len(), data.len()).as_bytes(),
                );
                out.extend_from_slice(data);
                out.extend_from_slice(b")\r\n");
            }
            out.extend_from_slice(format!("{tag} OK FETCH done\r\n").as_bytes());
        } else if upper.starts_with("LOGOUT") {
            out.extend_from_slice(format!("* BYE\r\n{tag} OK bye\r\n").as_bytes());
            let _ = w.write_all(&out);
            return;
        } else {
            out.extend_from_slice(format!("{tag} BAD unknown\r\n").as_bytes());
        }
        if w.write_all(&out).is_err() {
            return;
        }
    }
}

fn start(st: Arc<Mutex<Server>>) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let st = st.clone();
            std::thread::spawn(move || handle(s, st));
        }
    });
    port
}

#[test]
fn imap_incremental_dedupe_and_header_first() {
    let st = Arc::new(Mutex::new(Server::default()));
    {
        let mut s = st.lock().unwrap();
        let a = msg("a@x", "Alpha");
        s.folders.insert("INBOX".into(), (42, BTreeMap::from([(1, a.clone()), (2, msg("b@x", "Bravo"))])));
        s.folders.insert("Archive".into(), (9, BTreeMap::from([(7, a), (8, msg("c@x", "Charlie"))])));
        s.folders.insert("Junk".into(), (1, BTreeMap::from([(1, msg("spam@x", "Spam"))])));
    }
    let port = start(st.clone());
    let d = tempfile::tempdir().unwrap();
    let mut a = Archive::open(&d.path().join("arch"), &OpenOptions { cache_dir: Some(d.path().join("c")), writer: true, create: true, ..Default::default() }).unwrap();
    let cfg = ImapConfig {
        host: "127.0.0.1".into(),
        port,
        user: "u".into(),
        password: "p".into(),
        src: Some("mock".into()),
        folders: vec![],
        exclude: nea::import::imap::DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect(),
        plaintext: true,
    };
    let s1 = import(&mut a, &cfg, |_, _| {}).unwrap();
    assert_eq!(s1.added, 3, "{s1:?}");
    assert_eq!(s1.new_locations, 1, "Alpha also in Archive");
    assert_eq!(a.index.search("", None, 0, 10).unwrap().total, 3);
    assert_eq!(a.index.search("spam", None, 0, 10).unwrap().total, 0, "Junk excluded");
    assert_eq!(a.index.search("infolder:mock/Archive", None, 0, 10).unwrap().total, 2);
    {
        let s = st.lock().unwrap();
        // Folders run in LIST order (Archive first). INBOX's copy of Alpha
        // (uid 1) was matched by its header and never downloaded.
        let inbox_fetch: Vec<u32> = s.body_fetches.iter().filter(|f| f.0 == "INBOX").flat_map(|f| f.1.clone()).collect();
        assert_eq!(inbox_fetch, vec![2]);
        let total: usize = s.body_fetches.iter().map(|f| f.1.len()).sum();
        assert_eq!(total, 3, "each distinct message downloaded once");
    }

    // Second run: nothing new, nothing downloaded.
    let before = st.lock().unwrap().body_fetches.len();
    let s2 = import(&mut a, &cfg, |_, _| {}).unwrap();
    assert_eq!(s2.added + s2.new_locations, 0, "{s2:?}");
    assert_eq!(st.lock().unwrap().body_fetches.len(), before);

    // New mail arrives: only it is fetched.
    st.lock().unwrap().folders.get_mut("INBOX").unwrap().1.insert(3, msg("d@x", "Delta"));
    let s3 = import(&mut a, &cfg, |_, _| {}).unwrap();
    assert_eq!(s3.added, 1);
    let last = st.lock().unwrap().body_fetches.last().cloned().unwrap();
    assert_eq!(last, ("INBOX".to_string(), vec![3]));

    // UIDVALIDITY change: folder is rescanned, but nothing duplicates.
    st.lock().unwrap().folders.get_mut("INBOX").unwrap().0 = 43;
    let s4 = import(&mut a, &cfg, |_, _| {}).unwrap();
    assert_eq!(s4.added, 0);
    assert_eq!(a.index.search("", None, 0, 10).unwrap().total, 4);
}

#[test]
fn gmail_all_mail_once_with_labels() {
    let st = Arc::new(Mutex::new(Server { gmail: true, ..Default::default() }));
    {
        let mut s = st.lock().unwrap();
        s.folders.insert("INBOX".into(), (1, BTreeMap::from([(1, msg("a@x", "Alpha"))])));
        s.folders.insert("Travel".into(), (2, BTreeMap::from([(1, msg("b@x", "Bravo"))])));
        s.folders.insert(
            "[Gmail]/All Mail".into(),
            (3, BTreeMap::from([(10, msg("a@x", "Alpha")), (11, msg("b@x", "Bravo")), (12, msg("c@x", "Charlie"))])),
        );
        s.labels.insert(10, vec!["\\Inbox".into(), "\\Important".into()]);
        s.labels.insert(11, vec!["Travel/2024".into()]);
    }
    let port = start(st.clone());
    let d = tempfile::tempdir().unwrap();
    let mut a = Archive::open(&d.path().join("arch"), &OpenOptions { cache_dir: Some(d.path().join("c")), writer: true, create: true, ..Default::default() }).unwrap();
    let cfg = ImapConfig {
        host: "127.0.0.1".into(),
        port,
        user: "u".into(),
        password: "p".into(),
        src: Some("gmail".into()),
        folders: vec![],
        exclude: vec![],
        plaintext: true,
    };
    let s1 = import(&mut a, &cfg, |_, _| {}).unwrap();
    assert_eq!(s1.added, 3, "{s1:?}");
    let fetched: Vec<String> = st.lock().unwrap().body_fetches.iter().map(|f| f.0.clone()).collect();
    assert!(fetched.iter().all(|f| f == "[Gmail]/All Mail"), "only All Mail downloaded: {fetched:?}");
    assert_eq!(a.index.search("label:important", None, 0, 10).unwrap().total, 1);
    assert_eq!(a.index.search("infolder:gmail/INBOX", None, 0, 10).unwrap().total, 1);
    assert_eq!(a.index.search("infolder:gmail/Travel/2024", None, 0, 10).unwrap().total, 1);
    assert_eq!(a.index.search("infolder:gmail/Archived", None, 0, 10).unwrap().total, 1, "unlabelled mail");
}
