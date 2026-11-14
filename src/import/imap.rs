//! IMAP archiving: incremental (UIDVALIDITY + last UID per folder),
//! resumable (sync state committed after every batch).
//!
//! Archiving is read-only (EXAMINE + BODY.PEEK). Only when a delete rule is
//! configured does a later pass SELECT the folder and delete messages that
//! are (1) old enough, (2) not flagged, (3) read unless the rule says
//! otherwise, and (4) proven to be in the archive, by Message-ID and size or
//! by the SHA-256 of the full message.
//!
//! Gmail: archives "[Gmail]/All Mail" once and records each message's labels
//! (X-GM-LABELS) as its folders, so labelled mail downloads once.
//! Other servers: before downloading a folder's messages, fetches only their
//! Message-ID and size; messages already archived from another folder get a
//! new location without being downloaded again.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::archive::{ingest, sha256_hex, Archive, ImportStats};
use crate::import::mbox::header_value;
use crate::rules::{imap_date, DateWindow, DeleteRule};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(skip)]
    pub password: String,
    /// Source name recorded in the archive (default user@host).
    #[serde(default)]
    pub src: Option<String>,
    /// Only these folders (default: all).
    #[serde(default)]
    pub folders: Vec<String>,
    /// Case-insensitive substrings of folder names to skip.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Allow an unencrypted connection (local testing only).
    #[serde(default)]
    pub plaintext: bool,
    /// Archive only messages in this date window.
    #[serde(default)]
    pub window: DateWindow,
    /// Delete archived messages from the server afterwards.
    #[serde(default)]
    pub delete: Option<DeleteRule>,
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

fn uid_set(uids: &[u32]) -> String {
    uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",")
}

type Session = imap::Session<imap::Connection>;

fn search(session: &mut Session, q: &str) -> Result<Vec<u32>> {
    let mut v: Vec<u32> = session.uid_search(q).with_context(|| format!("UID SEARCH {q}"))?.into_iter().collect();
    v.sort_unstable();
    Ok(v)
}

pub fn import(archive: &mut Archive, cfg: &ImapConfig, mut progress: impl FnMut(&str, &ImportStats)) -> Result<ImportStats> {
    cfg.window.validate()?;
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

    let window_criteria = cfg.window.imap_criteria();
    let mut stats = ImportStats::default();
    for (folder, all_mail) in &selected {
        let folder = folder.clone();
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
        let all: Vec<u32> = search(&mut session, &format!("UID {start}:*"))?.into_iter().filter(|&u| u >= start).collect();
        let uids: Vec<u32> = if window_criteria.is_empty() {
            all.clone()
        } else {
            search(&mut session, &format!("UID {start}:*{window_criteria}"))?.into_iter().filter(|&u| u >= start).collect()
        };
        // Messages outside the window are not archived. Never advance the
        // sync position past one of them, so a later run (when "older than
        // N days" lets it through) still sees it.
        let in_window: HashSet<u32> = uids.iter().copied().collect();
        let first_skipped = all.iter().copied().find(|u| !in_window.contains(u));
        stats.skipped += (all.len() - uids.len()) as u64;
        let use_labels = gmail && *all_mail;
        for chunk in uids.chunks(BATCH) {
            let set = uid_set(chunk);
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
                let set = uid_set(&to_fetch);
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
            let mut last = *chunk.last().unwrap();
            if let Some(fs) = first_skipped {
                last = last.min(fs.saturating_sub(1));
            }
            archive.set_sync_state(&src, &folder, uv, last)?;
            archive.commit()?;
            progress(&folder, &stats);
        }
        if uids.is_empty() {
            if let Some(fs) = first_skipped {
                archive.set_sync_state(&src, &folder, uv, fs.saturating_sub(1).max(start.saturating_sub(1)))?;
            }
        }
    }
    archive.commit()?;

    if let Some(rule) = &cfg.delete {
        for (folder, _) in &selected {
            match delete_archived(archive, &mut session, folder, rule) {
                Ok(n) => stats.deleted += n,
                Err(e) => stats.fail(format!("{folder}: delete after archiving failed: {e:#}")),
            }
            progress(folder, &stats);
        }
    }
    let _ = session.logout();
    archive.record_import("imap", &format!("{src} ({})", cfg.host), &stats)?;
    archive.commit()?;
    Ok(stats)
}

/// Delete messages that the archive provably holds and the rule allows.
fn delete_archived(archive: &Archive, session: &mut Session, folder: &str, rule: &DeleteRule) -> Result<u64> {
    session.select(folder).with_context(|| format!("SELECT {folder}"))?;
    let mut q = format!("BEFORE {} UNFLAGGED UNDELETED", imap_date(rule.cutoff()));
    if !rule.include_unread {
        q.push_str(" SEEN");
    }
    let candidates = search(session, &q)?;
    let mut deletable = Vec::new();
    for chunk in candidates.chunks(BATCH) {
        let heads = session.uid_fetch(uid_set(chunk), "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])")?;
        let mut need_body = Vec::new();
        for f in heads.iter() {
            let Some(uid) = f.uid else { continue };
            // Belt and braces: never delete flagged mail even if the server
            // ignored UNFLAGGED.
            if f.flags().iter().any(|fl| matches!(fl, imap::types::Flag::Flagged)) {
                continue;
            }
            let mid = f.header().and_then(|h| header_value(h, "Message-ID")).map(|m| m.trim().trim_matches(['<', '>']).to_string());
            let proven = match (mid, f.size) {
                (Some(m), Some(sz)) if !m.is_empty() => archive.find_by_msgid_size(&m, sz as i64)?.is_some(),
                _ => false,
            };
            if proven {
                deletable.push(uid);
            } else {
                need_body.push(uid);
            }
        }
        if !need_body.is_empty() {
            let bodies = session.uid_fetch(uid_set(&need_body), "(UID BODY.PEEK[])")?;
            for f in bodies.iter() {
                if let (Some(uid), Some(b)) = (f.uid, f.body()) {
                    if archive.catalog.has_message(&sha256_hex(b))? {
                        deletable.push(uid);
                    }
                }
            }
        }
    }
    if deletable.is_empty() {
        return Ok(0);
    }
    deletable.sort_unstable();
    for chunk in deletable.chunks(BATCH) {
        session.uid_store(uid_set(chunk), "+FLAGS (\\Deleted)").context("UID STORE \\Deleted")?;
    }
    session.expunge().context("EXPUNGE")?;
    Ok(deletable.len() as u64)
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
