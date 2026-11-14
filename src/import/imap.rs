//! IMAP archiving: incremental (UIDVALIDITY + last UID per folder),
//! resumable (sync state committed after every batch), read-only (EXAMINE +
//! BODY.PEEK, so nothing on the server changes).
//!
//! Gmail: archives "[Gmail]/All Mail" once and records each message's labels
//! (X-GM-LABELS) as its folders, so labelled mail downloads once.
//! Other servers: before downloading a folder's messages, fetches only their
//! Message-ID and size; messages already archived from another folder get a
//! new location without being downloaded again.

use anyhow::{bail, Context, Result};

use crate::archive::{ingest, Archive, ImportStats};
use crate::import::mbox::header_value;

#[derive(Debug, Clone)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// Source name recorded in the archive (default user@host).
    pub src: Option<String>,
    /// Only these folders (default: all).
    pub folders: Vec<String>,
    /// Case-insensitive substrings of folder names to skip.
    pub exclude: Vec<String>,
    /// Allow an unencrypted connection (local testing only).
    pub plaintext: bool,
}

pub const DEFAULT_EXCLUDE: &[&str] = &["spam", "junk", "trash", "deleted items", "deleted messages", "bulk mail"];
const BATCH: usize = 200;

fn is_gmail(host: &str) -> bool {
    let h = host.to_lowercase();
    h.ends_with("gmail.com") || h.ends_with("googlemail.com")
}

fn gmail_label_to_folder(l: &str) -> Option<String> {
    let l = l.trim().trim_matches('"');
    Some(match l {
        "\\Inbox" => "INBOX".into(),
        "\\Sent" => "Sent".into(),
        "\\Draft" | "\\Drafts" => "Drafts".into(),
        "\\Important" => "Important".into(),
        "\\Starred" => "Starred".into(),
        "\\Spam" | "\\Trash" => return None,
        x if x.starts_with('\\') => x[1..].to_string(),
        x => x.to_string(),
    })
}

pub fn import(archive: &mut Archive, cfg: &ImapConfig, mut progress: impl FnMut(&str, &ImportStats)) -> Result<ImportStats> {
    let src = cfg.src.clone().unwrap_or_else(|| format!("{}@{}", cfg.user, cfg.host).to_lowercase());
    let mode = if cfg.plaintext { imap::ConnectionMode::Plaintext } else { imap::ConnectionMode::AutoTls };
    let client = imap::ClientBuilder::new(cfg.host.as_str(), cfg.port)
        .mode(mode)
        .tls_kind(imap::TlsKind::Rust)
        .connect()
        .with_context(|| format!("connecting to {}:{}", cfg.host, cfg.port))?;
    let mut session = client.login(&cfg.user, &cfg.password).map_err(|(e, _)| e).context("IMAP login failed")?;

    let names = session.list(None, Some("*")).context("LIST")?;
    let mut folders: Vec<(String, bool)> = Vec::new(); // (name, is_all_mail)
    for n in names.iter() {
        let attrs: Vec<String> = n.attributes().iter().map(|a| format!("{a:?}")).collect();
        if attrs.iter().any(|a| a == "NoSelect") {
            continue;
        }
        folders.push((n.name().to_string(), attrs.iter().any(|a| a == "All")));
    }
    // Gmail (or a server offering Gmail's extension) with an \All folder.
    let gm_ext = session.capabilities().map(|c| c.has_str("X-GM-EXT-1")).unwrap_or(false);
    let gmail = (is_gmail(&cfg.host) || gm_ext) && folders.iter().any(|f| f.1);
    let selected: Vec<(String, bool)> = if !cfg.folders.is_empty() {
        folders.into_iter().filter(|(n, _)| cfg.folders.iter().any(|w| w.eq_ignore_ascii_case(n))).collect()
    } else if gmail {
        folders.into_iter().filter(|f| f.1).collect()
    } else {
        folders
            .into_iter()
            .filter(|(n, _)| {
                let l = n.to_lowercase();
                !cfg.exclude.iter().any(|x| l.contains(&x.to_lowercase()))
            })
            .collect()
    };
    if selected.is_empty() {
        bail!("no folders to archive on {}", cfg.host);
    }

    let mut stats = ImportStats::default();
    for (folder, all_mail) in selected {
        let mbox = match session.examine(&folder) {
            Ok(m) => m,
            Err(e) => {
                stats.fail(format!("{folder}: EXAMINE failed: {e}"));
                continue;
            }
        };
        let uv = mbox.uid_validity.unwrap_or(0);
        let start = match archive.catalog.get_sync(&src, &folder)? {
            Some((v, last)) if v == uv => last + 1,
            _ => 1,
        };
        if mbox.exists == 0 {
            archive.set_sync_state(&src, &folder, uv, start.saturating_sub(1))?;
            continue;
        }
        let mut uids: Vec<u32> = session
            .uid_search(format!("UID {start}:*"))
            .with_context(|| format!("{folder}: UID SEARCH"))?
            .into_iter()
            .filter(|&u| u >= start)
            .collect();
        uids.sort_unstable();
        let use_labels = gmail && all_mail;
        for chunk in uids.chunks(BATCH) {
            let set = chunk.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
            let mut to_fetch: Vec<u32> = chunk.to_vec();
            // Header-first: skip downloading messages archived from another folder.
            if !use_labels {
                let heads = session.uid_fetch(&set, "(UID RFC822.SIZE BODY.PEEK[HEADER])")?;
                to_fetch.clear();
                for f in heads.iter() {
                    let Some(uid) = f.uid else { continue };
                    let mid = f
                        .header()
                        .or_else(|| f.body())
                        .and_then(|h| header_value(h, "Message-ID"))
                        .map(|m| m.trim().trim_matches(['<', '>']).to_string());
                    let known = match (mid, f.size) {
                        (Some(m), Some(sz)) if !m.is_empty() => archive.find_by_msgid_size(&m, sz as i64)?,
                        _ => None,
                    };
                    match known {
                        Some(id) => stats.record(archive.add_location_existing(&id, &src, &folder)?),
                        None => to_fetch.push(uid),
                    }
                }
                to_fetch.sort_unstable();
            }
            if !to_fetch.is_empty() {
                let set = to_fetch.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
                let query = if use_labels { "(UID X-GM-LABELS BODY.PEEK[])" } else { "(UID BODY.PEEK[])" };
                let fetches = session.uid_fetch(&set, query).with_context(|| format!("{folder}: UID FETCH"))?;
                let mut items = Vec::with_capacity(fetches.len());
                for f in fetches.iter() {
                    let Some(body) = f.body() else {
                        items.push(Err(format!("{folder} uid {:?}: no body returned", f.uid)));
                        continue;
                    };
                    let folders = if use_labels {
                        let mut v: Vec<String> = f.gmail_labels().map(|it| it.filter_map(gmail_label_to_folder).collect()).unwrap_or_default();
                        if v.is_empty() {
                            v.push("Archived".into());
                        }
                        v
                    } else {
                        vec![folder.clone()]
                    };
                    items.push(Ok((body.to_vec(), src.clone(), folders)));
                }
                ingest(archive, items.into_iter(), &mut stats, |_| {})?;
            }
            let last = *chunk.last().unwrap();
            archive.set_sync_state(&src, &folder, uv, last)?;
            archive.commit()?;
            progress(&folder, &stats);
        }
    }
    let _ = session.logout();
    archive.record_import("imap", &format!("{src} ({})", cfg.host), &stats)?;
    archive.commit()?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_labels_map() {
        assert_eq!(gmail_label_to_folder("\\Inbox").unwrap(), "INBOX");
        assert_eq!(gmail_label_to_folder("\"Receipts/2024\"").unwrap(), "Receipts/2024");
        assert!(gmail_label_to_folder("\\Trash").is_none());
    }
}
