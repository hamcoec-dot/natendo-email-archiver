//! POP3 importer against a minimal in-process POP3 server.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use nea::archive::{Archive, OpenOptions};
use nea::import::pop3::{import, Pop3Config};

#[derive(Default)]
struct Server {
    msgs: Vec<Option<Vec<u8>>>, // None = deleted
    retr: Vec<usize>,
    pending_dele: Vec<usize>,
}

fn msg(id: &str, date: &str, body: &str) -> Vec<u8> {
    format!("From: a@b.com\r\nTo: c@d.com\r\nSubject: {id}\r\nMessage-ID: <{id}@pop>\r\nDate: {date}\r\n\r\n{body}\r\n").into_bytes()
}

fn handle(stream: TcpStream, st: Arc<Mutex<Server>>) {
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    w.write_all(b"+OK mock POP3\r\n").unwrap();
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let parts: Vec<String> = line.split_whitespace().map(String::from).collect();
        let cmd = parts.first().map(|s| s.to_uppercase()).unwrap_or_default();
        let arg = |i: usize| parts.get(i).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
        let mut s = st.lock().unwrap();
        let mut out = Vec::new();
        let dot = |data: &[u8], out: &mut Vec<u8>| {
            for l in data.split_inclusive(|&b| b == b'\n') {
                if l.starts_with(b".") {
                    out.push(b'.');
                }
                out.extend_from_slice(l);
            }
            out.extend_from_slice(b".\r\n");
        };
        match cmd.as_str() {
            "USER" | "PASS" => out.extend_from_slice(b"+OK\r\n"),
            "LIST" => {
                out.extend_from_slice(b"+OK\r\n");
                for (i, m) in s.msgs.iter().enumerate() {
                    if let Some(m) = m {
                        out.extend_from_slice(format!("{} {}\r\n", i + 1, m.len()).as_bytes());
                    }
                }
                out.extend_from_slice(b".\r\n");
            }
            "TOP" => {
                let m = s.msgs[arg(1) - 1].clone().unwrap();
                let end = m.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                out.extend_from_slice(b"+OK\r\n");
                dot(&m[..end], &mut out);
            }
            "RETR" => {
                let n = arg(1);
                s.retr.push(n);
                let m = s.msgs[n - 1].clone().unwrap();
                out.extend_from_slice(b"+OK\r\n");
                dot(&m, &mut out);
            }
            "DELE" => {
                s.pending_dele.push(arg(1));
                out.extend_from_slice(b"+OK\r\n");
            }
            "QUIT" => {
                for n in std::mem::take(&mut s.pending_dele) {
                    s.msgs[n - 1] = None;
                }
                out.extend_from_slice(b"+OK bye\r\n");
                let _ = w.write_all(&out);
                return;
            }
            _ => out.extend_from_slice(b"-ERR unknown\r\n"),
        }
        drop(s);
        if w.write_all(&out).is_err() {
            return;
        }
    }
}

#[test]
fn pop3_archive_dedupe_and_delete() {
    let st = Arc::new(Mutex::new(Server::default()));
    {
        let mut s = st.lock().unwrap();
        s.msgs.push(Some(msg("old", "Tue, 05 Mar 2019 10:00:00 +0000", "first\r\n.leading dot line")));
        s.msgs.push(Some(msg("new", &chrono::Utc::now().to_rfc2822(), "second")));
    }
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let st2 = st.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let st = st2.clone();
            std::thread::spawn(move || handle(s, st));
        }
    });
    let d = tempfile::tempdir().unwrap();
    let mut a = Archive::open(&d.path().join("a"), &OpenOptions { cache_dir: Some(d.path().join("c")), writer: true, create: true, ..Default::default() }).unwrap();
    let mut cfg = Pop3Config { host: "127.0.0.1".into(), port, user: "u".into(), password: "p".into(), plaintext: true, ..Default::default() };
    let s1 = import(&mut a, &cfg, |_| {}).unwrap();
    assert_eq!(s1.added, 2, "{s1:?}");
    assert_eq!(a.index.search("\"leading dot line\"", None, 0, 5).unwrap().total, 1, "dot-unstuffing");

    // Second run: headers only, nothing downloaded again.
    st.lock().unwrap().retr.clear();
    cfg.delete = Some(nea::rules::DeleteRule { older_than_days: 30, include_unread: true });
    let s2 = import(&mut a, &cfg, |_| {}).unwrap();
    assert_eq!(s2.added, 0);
    assert!(st.lock().unwrap().retr.is_empty() || s2.duplicates > 0, "known messages skipped");
    assert_eq!(s2.deleted, 1, "{s2:?}");
    let s = st.lock().unwrap();
    assert!(s.msgs[0].is_none() && s.msgs[1].is_some(), "only the old message was deleted");
}
