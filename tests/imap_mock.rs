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
    /// (folder, uid) -> (internal date, flagged, seen). Default: 400 days old, unflagged, seen.
    meta: BTreeMap<(String, u32), (i64, bool, bool)>,
    /// \Deleted flags set but not yet expunged.
    deleted_flags: Vec<(String, u32)>,
    /// Messages removed by EXPUNGE.
    expunged: Vec<(String, u32)>,
    /// APPEND commands received: (folder, command line).
    appends: Vec<(String, String)>,
}

impl Server {
    fn meta_of(&self, folder: &str, uid: u32) -> (i64, bool, bool) {
        self.meta.get(&(folder.to_string(), uid)).copied().unwrap_or((chrono::Utc::now().timestamp() - 400 * 86_400, false, true))
    }
}

fn imap_day(s: &str) -> i64 {
    chrono::NaiveDate::parse_from_str(s, "%d-%b-%Y").unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp()
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
            // Supports: UID a:* | a:b, SINCE d, BEFORE d, UNFLAGGED, SEEN, UNDELETED
            let toks: Vec<&str> = rest.split_whitespace().skip(2).collect();
            let s = st.lock().unwrap();
            let msgs = &s.folders[&selected].1;
            let mut uids: Vec<u32> = msgs.keys().copied().collect();
            let mut i = 0;
            let mut uid_range = false;
            while i < toks.len() {
                match toks[i].to_uppercase().as_str() {
                    "UID" => {
                        i += 1;
                        let (a, b) = toks[i].split_once(':').unwrap();
                        let a: u32 = a.parse().unwrap();
                        let b: u32 = if b == "*" { u32::MAX } else { b.parse().unwrap() };
                        uids.retain(|&u| u >= a && u <= b);
                        uid_range = b == u32::MAX;
                    }
                    "SINCE" => {
                        i += 1;
                        let d = imap_day(toks[i]);
                        uids.retain(|&u| s.meta_of(&selected, u).0 >= d);
                    }
                    "BEFORE" => {
                        i += 1;
                        let d = imap_day(toks[i]);
                        uids.retain(|&u| s.meta_of(&selected, u).0 < d);
                    }
                    "UNFLAGGED" => uids.retain(|&u| !s.meta_of(&selected, u).1),
                    "SEEN" => uids.retain(|&u| s.meta_of(&selected, u).2),
                    "UNDELETED" => uids.retain(|u| !s.deleted_flags.contains(&(selected.clone(), *u))),
                    _ => {}
                }
                i += 1;
            }
            if uids.is_empty() && uid_range && toks.len() == 2 {
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
                let flags = if upper.contains("FLAGS") {
                    let (_, f, seen) = s.meta_of(&selected, *uid);
                    let mut v = Vec::new();
                    if seen {
                        v.push("\\Seen");
                    }
                    if f {
                        v.push("\\Flagged");
                    }
                    format!(" FLAGS ({})", v.join(" "))
                } else {
                    String::new()
                };
                out.extend_from_slice(
                    format!("* {} FETCH (UID {uid}{labels}{flags} RFC822.SIZE {} {section} {{{}}}\r\n", seq + 1, raw.len(), data.len()).as_bytes(),
                );
                out.extend_from_slice(data);
                out.extend_from_slice(b")\r\n");
            }
            out.extend_from_slice(format!("{tag} OK FETCH done\r\n").as_bytes());
        } else if upper.starts_with("UID STORE") {
            let parts: Vec<&str> = rest.splitn(4, ' ').collect();
            let mut s = st.lock().unwrap();
            for u in parse_set(parts[2]) {
                s.deleted_flags.push((selected.clone(), u));
            }
            out.extend_from_slice(format!("{tag} OK STORE done\r\n").as_bytes());
        } else if upper.starts_with("EXPUNGE") {
            let mut s = st.lock().unwrap();
            let doomed: Vec<(String, u32)> = s.deleted_flags.iter().filter(|(f, _)| *f == selected).cloned().collect();
            for (f, u) in &doomed {
                let seq = s.folders[f].1.keys().position(|k| k == u).unwrap() + 1;
                s.folders.get_mut(f).unwrap().1.remove(u);
                out.extend_from_slice(format!("* {seq} EXPUNGE\r\n").as_bytes());
                s.expunged.push((f.clone(), *u));
            }
            s.deleted_flags.retain(|(f, _)| *f != selected);
            out.extend_from_slice(format!("{tag} OK EXPUNGE done\r\n").as_bytes());
        } else if upper.starts_with("CREATE") {
            let name = rest[7..].trim().trim_matches('"').to_string();
            st.lock().unwrap().folders.entry(name).or_insert((1, BTreeMap::new()));
            out.extend_from_slice(format!("{tag} OK CREATE done\r\n").as_bytes());
        } else if upper.starts_with("APPEND") {
            // APPEND "folder" (flags) "date" {n}
            let folder = rest[7..].trim_start().trim_start_matches('"').split('"').next().unwrap().to_string();
            let n: usize = l.rsplit('{').next().unwrap().trim_end_matches('}').parse().unwrap();
            w.write_all(b"+ Ready\r\n").unwrap();
            let mut data = vec![0u8; n];
            std::io::Read::read_exact(&mut r, &mut data).unwrap();
            let mut crlf = String::new();
            r.read_line(&mut crlf).unwrap();
            let mut s = st.lock().unwrap();
            s.appends.push((folder.clone(), l.to_string()));
            let msgs = &mut s.folders.get_mut(&folder).expect("APPEND to a missing folder").1;
            let uid = msgs.keys().last().copied().unwrap_or(0) + 1;
            msgs.insert(uid, data);
            out.extend_from_slice(format!("{tag} OK APPEND done\r\n").as_bytes());
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
        ..Default::default()
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
        ..Default::default()
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

#[test]
fn imap_date_window_and_delete_after_archiving() {
    let now = chrono::Utc::now().timestamp();
    let st = Arc::new(Mutex::new(Server::default()));
    {
        let mut s = st.lock().unwrap();
        s.folders.insert(
            "INBOX".into(),
            (5, BTreeMap::from([(1, msg("old@x", "Old")), (2, msg("flag@x", "Flagged old")), (3, msg("unread@x", "Unread old")), (4, msg("new@x", "New"))])),
        );
        s.meta.insert(("INBOX".into(), 2), (now - 400 * 86_400, true, true));
        s.meta.insert(("INBOX".into(), 3), (now - 400 * 86_400, false, false));
        s.meta.insert(("INBOX".into(), 4), (now - 86_400, false, true));
    }
    let port = start(st.clone());
    let d = tempfile::tempdir().unwrap();
    let mut a = Archive::open(&d.path().join("arch"), &OpenOptions { cache_dir: Some(d.path().join("c")), writer: true, create: true, ..Default::default() }).unwrap();
    let mut cfg = ImapConfig {
        host: "127.0.0.1".into(),
        port,
        user: "u".into(),
        password: "p".into(),
        src: Some("mock".into()),
        plaintext: true,
        window: nea::rules::DateWindow { older_than_days: Some(30), ..Default::default() },
        delete: Some(nea::rules::DeleteRule { older_than_days: 365, include_unread: false }),
        ..Default::default()
    };
    let s1 = import(&mut a, &cfg, |_, _| {}).unwrap();
    assert_eq!(s1.added, 3, "new mail is outside the 'older than 30 days' window: {s1:?}");
    assert_eq!(s1.skipped, 1);
    assert_eq!(s1.deleted, 1, "{s1:?}");
    assert_eq!(st.lock().unwrap().expunged, vec![("INBOX".to_string(), 1)], "only the old, read, unflagged message");

    // The skipped new message is still picked up once the window allows it.
    cfg.window = Default::default();
    cfg.delete = None;
    let s2 = import(&mut a, &cfg, |_, _| {}).unwrap();
    assert_eq!(s2.added, 1, "{s2:?}");
    assert_eq!(a.index.search("", None, 0, 10).unwrap().total, 4);
}

#[test]
fn export_to_imap_restores_folders_once() {
    use nea::deliver::{to_imap, ImapTarget};
    let st = Arc::new(Mutex::new(Server::default()));
    st.lock().unwrap().folders.insert("INBOX".into(), (1, BTreeMap::from([(1, msg("there@x", "Already there"))])));
    let port = start(st.clone());
    let d = tempfile::tempdir().unwrap();
    let mut a = Archive::open(&d.path().join("arch"), &OpenOptions { cache_dir: Some(d.path().join("c")), writer: true, create: true, ..Default::default() }).unwrap();
    a.add(&msg("one@x", "One"), "old", &["Projects/2019".to_string()]).unwrap();
    a.add(&msg("two@x", "Two"), "old", &["INBOX".to_string()]).unwrap();
    a.add(&msg("there@x", "Already there"), "old", &["INBOX".to_string()]).unwrap();
    a.commit().unwrap();
    let ids = a.index.all_ids("").unwrap();
    let t = ImapTarget { host: "127.0.0.1".into(), port, user: "u".into(), password: "p".into(), plaintext: true, ..Default::default() };
    let s1 = to_imap(&a, &ids, &t, |_| {}).unwrap();
    assert_eq!((s1.sent, s1.skipped, s1.failed), (2, 1, 0), "{s1:?}");
    {
        let s = st.lock().unwrap();
        assert_eq!(s.folders["Projects/2019"].1.len(), 1);
        assert!(s.folders.contains_key("Projects"), "parent created");
        assert_eq!(s.folders["INBOX"].1.len(), 2);
        let (_, cmd) = &s.appends[0];
        assert!(cmd.contains("(\\Seen)") && cmd.contains("05-Mar-2019"), "{cmd}");
    }
    // Re-running copies nothing twice.
    let s2 = to_imap(&a, &ids, &t, |_| {}).unwrap();
    assert_eq!((s2.sent, s2.skipped), (0, 3), "{s2:?}");
    // Everything into one folder.
    let t2 = ImapTarget { folder: Some("Restored".into()), ..t };
    let s3 = to_imap(&a, &ids, &t2, |_| {}).unwrap();
    assert_eq!(s3.sent, 3);
}
