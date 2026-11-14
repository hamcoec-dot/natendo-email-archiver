//! Sending archived mail by SMTP, against a minimal in-process SMTP server.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use nea::archive::{Archive, OpenOptions};
use nea::deliver::{to_smtp, Security, SmtpTarget};

#[derive(Default)]
struct Got {
    auth: Vec<String>,
    /// (mail from, rcpts, data without the final dot line)
    mails: Vec<(String, Vec<String>, Vec<u8>)>,
}

fn serve(got: Arc<Mutex<Got>>) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let got = got.clone();
            std::thread::spawn(move || {
                let mut w = s.try_clone().unwrap();
                let mut r = BufReader::new(s);
                w.write_all(b"220 mock ESMTP\r\n").unwrap();
                let (mut from, mut rcpt) = (String::new(), Vec::new());
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let u = line.to_uppercase();
                    let reply: &[u8] = if u.starts_with("EHLO") {
                        b"250-mock\r\n250-SIZE 1000000\r\n250 AUTH PLAIN LOGIN\r\n"
                    } else if u.starts_with("AUTH PLAIN") {
                        got.lock().unwrap().auth.push(line.trim().to_string());
                        b"235 ok\r\n"
                    } else if u.starts_with("MAIL FROM:") {
                        from = line[10..].trim().to_string();
                        b"250 ok\r\n"
                    } else if u.starts_with("RCPT TO:") {
                        if line.contains("nobody@") {
                            b"550 no such user\r\n"
                        } else {
                            rcpt.push(line[8..].trim().to_string());
                            b"250 ok\r\n"
                        }
                    } else if u.starts_with("DATA") {
                        w.write_all(b"354 go\r\n").unwrap();
                        let mut data = Vec::new();
                        loop {
                            let mut l = Vec::new();
                            r.read_until(b'\n', &mut l).unwrap();
                            if l == b".\r\n" {
                                break;
                            }
                            data.extend_from_slice(&l);
                        }
                        got.lock().unwrap().mails.push((std::mem::take(&mut from), std::mem::take(&mut rcpt), data));
                        b"250 queued\r\n"
                    } else if u.starts_with("RSET") {
                        from.clear();
                        rcpt.clear();
                        b"250 ok\r\n"
                    } else if u.starts_with("QUIT") {
                        w.write_all(b"221 bye\r\n").unwrap();
                        return;
                    } else {
                        b"500 what\r\n"
                    };
                    w.write_all(reply).unwrap();
                }
            });
        }
    });
    port
}

#[test]
fn sends_archived_mail_unchanged() {
    let got = Arc::new(Mutex::new(Got::default()));
    let port = serve(got.clone());
    let d = tempfile::tempdir().unwrap();
    let mut a = Archive::open(&d.path().join("a"), &OpenOptions { cache_dir: Some(d.path().join("c")), writer: true, create: true, ..Default::default() }).unwrap();
    let raw = b"From: x@y.com\nSubject: Dots\nMessage-ID: <dots@x>\n\n.starts with a dot\nend\n".to_vec();
    a.add(&raw, "t", &["INBOX".to_string()]).unwrap();
    a.commit().unwrap();
    let ids = a.index.all_ids("").unwrap();
    let mut t = SmtpTarget {
        host: "127.0.0.1".into(),
        port,
        security: Security::Plain,
        user: Some("me".into()),
        password: "pw".into(),
        from: "me@example.com".into(),
        to: vec!["archive@example.com".into()],
    };
    let s = to_smtp(&a, &ids, &t, |_| {}).unwrap();
    assert_eq!((s.sent, s.failed), (1, 0), "{s:?}");
    {
        let g = got.lock().unwrap();
        assert_eq!(g.auth.len(), 1);
        let (from, to, data) = &g.mails[0];
        assert!(from.starts_with("<me@example.com>"), "{from}");
        assert_eq!(to, &vec!["<archive@example.com>".to_string()]);
        // Dot-stuffed on the wire; CRLF line endings.
        assert_eq!(data, &b"From: x@y.com\r\nSubject: Dots\r\nMessage-ID: <dots@x>\r\n\r\n..starts with a dot\r\nend\r\n".to_vec());
    }
    // A refused recipient fails that message and the session continues.
    t.to = vec!["nobody@example.com".into()];
    let s = to_smtp(&a, &ids, &t, |_| {}).unwrap();
    assert_eq!((s.sent, s.failed), (0, 1));
    assert!(s.errors[0].contains("550"), "{:?}", s.errors);
    // Header injection in addresses is refused.
    t.to = vec!["a@b.com>\r\nRCPT TO:<c@d.com".into()];
    assert!(to_smtp(&a, &ids, &t, |_| {}).is_err());
}
