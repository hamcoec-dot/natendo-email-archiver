//! Archive straight from the Outlook profile on this PC (classic Outlook,
//! Windows), like MailStore Home's "Microsoft Outlook" source: choose
//! mailboxes and folders, limit by date, and optionally delete mail from
//! Outlook once it is safely archived (flagged mail is never deleted; deleted
//! items go to Outlook's Deleted Items folder).
//!
//! Outlook is driven through its COM automation interface by a bundled
//! PowerShell script (`scripts/outlook-bridge.ps1`), which saves each item as
//! a .msg file that the .msg importer converts. The [`Bridge`] trait lets
//! tests replace Outlook with a fake.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::archive::{ingest, sha256_hex, Archive, ImportStats};
use crate::rules::{DateWindow, DeleteRule};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutlookConfig {
    /// Mailbox / data file display names (empty: all).
    #[serde(default)]
    pub stores: Vec<String>,
    /// Folder paths like "Inbox" or "Inbox/Receipts" (empty: all mail folders);
    /// a folder includes its subfolders.
    #[serde(default)]
    pub folders: Vec<String>,
    /// Skip folders whose path contains any of these (default: deleted items, junk).
    #[serde(default = "default_exclude")]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub window: DateWindow,
    #[serde(default)]
    pub delete: Option<DeleteRule>,
    /// Re-scan everything instead of only mail received since the last run.
    #[serde(default)]
    pub full_rescan: bool,
}

pub fn default_exclude() -> Vec<String> {
    vec!["Deleted Items".into(), "Junk Email".into(), "Junk E-mail".into(), "Sync Issues".into(), "Conversation History".into()]
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeLine {
    Folder { store: String, path: String, count: i64 },
    Item {
        file: PathBuf,
        #[serde(rename = "entryId")]
        entry_id: String,
        #[serde(rename = "storeId")]
        store_id: String,
        store: String,
        path: String,
        received: i64,
        flagged: bool,
        unread: bool,
    },
    Warning { message: String },
    Error { message: String },
    Done {
        #[serde(default)]
        count: Option<i64>,
        #[serde(default)]
        deleted: Option<i64>,
        #[serde(default)]
        kept: Option<i64>,
    },
}

pub struct ExportRequest {
    pub out_dir: PathBuf,
    pub since: Option<i64>,
    pub before: Option<i64>,
    pub stores: Vec<String>,
    pub folders: Vec<String>,
    pub exclude: Vec<String>,
}

pub trait Bridge: Sync {
    /// Stream exported items (and warnings) through `tx`.
    fn export(&self, req: &ExportRequest, tx: mpsc::SyncSender<BridgeLine>) -> Result<()>;
    /// Delete these (entryId, storeId) items; returns (deleted, kept-because-flagged).
    fn delete(&self, items: &[(String, String)]) -> Result<(u64, u64)>;
    fn list(&self) -> Result<Vec<BridgeLine>>;
}

/// The real bridge: PowerShell + Outlook COM.
pub struct PowerShellBridge {
    script: PathBuf,
}

const SCRIPT: &str = include_str!("../../scripts/outlook-bridge.ps1");

impl PowerShellBridge {
    pub fn new() -> Result<Self> {
        if !cfg!(windows) {
            bail!("archiving from an Outlook profile needs classic Outlook on Windows; on other systems import a .pst file instead");
        }
        let dir = std::env::temp_dir().join("natendo-email-archiver");
        std::fs::create_dir_all(&dir)?;
        let script = dir.join(format!("outlook-bridge-{}.ps1", crate::archive::sha256_hex(SCRIPT.as_bytes()).get(..12).unwrap_or("x")));
        if !script.exists() {
            // UTF-8 with BOM so Windows PowerShell 5 reads non-ASCII correctly.
            let mut data = vec![0xEF, 0xBB, 0xBF];
            data.extend_from_slice(SCRIPT.as_bytes());
            std::fs::write(&script, data)?;
        }
        Ok(Self { script })
    }

    fn command(&self, args: &[String]) -> Command {
        let mut c = Command::new("powershell.exe");
        c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]).arg(&self.script).args(args);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        c
    }

    fn run_lines(&self, args: &[String], mut on: impl FnMut(BridgeLine) -> bool) -> Result<()> {
        let mut child = self.command(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("starting PowerShell")?;
        let out = BufReader::new(child.stdout.take().unwrap());
        let mut error = None;
        for line in out.lines() {
            let line = line?;
            let Ok(parsed) = serde_json::from_str::<BridgeLine>(line.trim_start_matches('\u{feff}')) else { continue };
            if let BridgeLine::Error { message } = &parsed {
                error = Some(message.clone());
            }
            if !on(parsed) {
                break;
            }
        }
        let status = child.wait()?;
        if let Some(e) = error {
            bail!("{e}");
        }
        if !status.success() {
            let mut err = String::new();
            if let Some(mut s) = child.stderr.take() {
                use std::io::Read;
                let _ = s.read_to_string(&mut err);
            }
            bail!("Outlook bridge failed: {}", err.lines().take(5).collect::<Vec<_>>().join(" "));
        }
        Ok(())
    }
}

impl Bridge for PowerShellBridge {
    fn export(&self, req: &ExportRequest, tx: mpsc::SyncSender<BridgeLine>) -> Result<()> {
        let mut args = vec!["export".to_string(), "-OutDir".into(), req.out_dir.display().to_string()];
        if let Some(s) = req.since {
            args.extend(["-Since".into(), s.to_string()]);
        }
        if let Some(b) = req.before {
            args.extend(["-Before".into(), b.to_string()]);
        }
        for (flag, v) in [("-Stores", &req.stores), ("-Folders", &req.folders), ("-Exclude", &req.exclude)] {
            if !v.is_empty() {
                args.extend([flag.to_string(), v.join("|")]);
            }
        }
        self.run_lines(&args, |l| tx.send(l).is_ok())
    }

    fn delete(&self, items: &[(String, String)]) -> Result<(u64, u64)> {
        let list = std::env::temp_dir().join(format!("nea-outlook-delete-{}.txt", std::process::id()));
        std::fs::write(&list, items.iter().map(|(e, s)| format!("{e}\t{s}\n")).collect::<String>())?;
        let mut result = (0, 0);
        let r = self.run_lines(&["delete".into(), "-List".into(), list.display().to_string()], |l| {
            if let BridgeLine::Done { deleted, kept, .. } = l {
                result = (deleted.unwrap_or(0) as u64, kept.unwrap_or(0) as u64);
            }
            true
        });
        let _ = std::fs::remove_file(&list);
        r.map(|_| result)
    }

    fn list(&self) -> Result<Vec<BridgeLine>> {
        let mut v = Vec::new();
        self.run_lines(&["list".into()], |l| {
            v.push(l);
            true
        })?;
        Ok(v)
    }
}

fn source_name(store: &str) -> String {
    format!("outlook:{store}")
}

pub fn import(archive: &mut Archive, cfg: &OutlookConfig, progress: impl FnMut(&str, &ImportStats)) -> Result<ImportStats> {
    let bridge = PowerShellBridge::new()?;
    import_with(archive, cfg, &bridge, progress)
}

/// Sync-state key: one marker per profile configuration, recording the
/// newest received time archived (the "UID" slot holds Unix seconds).
fn state_key(cfg: &OutlookConfig) -> String {
    format!("stores={}|folders={}", cfg.stores.join(","), cfg.folders.join(","))
}

pub fn import_with(archive: &mut Archive, cfg: &OutlookConfig, bridge: &dyn Bridge, mut progress: impl FnMut(&str, &ImportStats)) -> Result<ImportStats> {
    cfg.window.validate()?;
    let (lo, hi) = cfg.window.bounds_now();
    let key = state_key(cfg);
    let last = if cfg.full_rescan { None } else { archive.catalog.get_sync("outlook", &key)?.map(|(_, t)| t as i64) };
    let since = match (lo, last) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    static RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let run = RUN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let out_dir = std::env::temp_dir().join(format!("nea-outlook-{}-{run}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir)?;
    let req = ExportRequest {
        out_dir: out_dir.clone(),
        since,
        before: hi,
        stores: cfg.stores.clone(),
        folders: cfg.folders.clone(),
        exclude: cfg.exclude.clone(),
    };

    let mut stats = ImportStats::default();
    // Items seen this run: (eml sha256, entryId, storeId, received, flagged, unread)
    let seen = std::sync::Mutex::new(Vec::<(String, String, String, i64, bool, bool)>::new());
    let mut newest = last.unwrap_or(0);
    let (tx, rx) = mpsc::sync_channel::<BridgeLine>(64);
    let warnings = std::sync::Mutex::new(Vec::<String>::new());
    std::thread::scope(|s| -> Result<()> {
        let export = s.spawn(|| bridge.export(&req, tx));
        let items = rx.into_iter().filter_map(|line| match line {
            BridgeLine::Item { file, entry_id, store_id, store, path, received, flagged, unread } => {
                let r = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display())).and_then(|data| {
                    crate::import::msg::msg_to_eml_bytes(&data).map_err(|e| format!("{store}/{path}: {e:#}"))
                });
                let _ = std::fs::remove_file(&file);
                Some(r.map(|eml| {
                    seen.lock().unwrap().push((sha256_hex(&eml), entry_id, store_id, received, flagged, unread));
                    (eml, source_name(&store), vec![path])
                }))
            }
            BridgeLine::Warning { message } => {
                warnings.lock().unwrap().push(message);
                None
            }
            _ => None,
        });
        ingest(archive, items, &mut stats, |st| progress("Outlook", st))?;
        export.join().map_err(|_| anyhow::anyhow!("Outlook bridge thread panicked"))??;
        Ok(())
    })?;
    let _ = std::fs::remove_dir_all(&out_dir);
    for w in warnings.into_inner().unwrap() {
        stats.fail(w);
    }
    let seen = seen.into_inner().unwrap();
    for (_, _, _, received, _, _) in &seen {
        newest = newest.max(*received);
    }
    if newest > 0 {
        archive.set_sync_state("outlook", &key, 0, newest.min(u32::MAX as i64) as u32)?;
    }
    archive.commit()?;

    if let Some(rule) = &cfg.delete {
        let cutoff = rule.cutoff();
        let mut doomed = Vec::new();
        for (sha, entry, store, received, flagged, unread) in &seen {
            if *flagged || (*unread && !rule.include_unread) || *received <= 0 || *received >= cutoff {
                continue;
            }
            // Only what the archive provably holds, byte for byte.
            if archive.catalog.has_message(sha)? {
                doomed.push((entry.clone(), store.clone()));
            }
        }
        if !doomed.is_empty() {
            let (deleted, _kept) = bridge.delete(&doomed)?;
            stats.deleted += deleted;
        }
    }
    archive.record_import("outlook", &format!("Outlook profile ({})", if cfg.stores.is_empty() { "all mailboxes".into() } else { cfg.stores.join(", ") }), &stats)?;
    archive.commit()?;
    progress("Outlook", &stats);
    Ok(stats)
}

/// Mailboxes and mail folders in the Outlook profile.
pub fn list_folders() -> Result<Vec<(String, String, i64)>> {
    let bridge = PowerShellBridge::new()?;
    Ok(bridge
        .list()?
        .into_iter()
        .filter_map(|l| match l {
            BridgeLine::Folder { store, path, count } => Some((store, path, count)),
            _ => None,
        })
        .collect())
}

pub fn script_path_for_tests() -> Option<PathBuf> {
    PowerShellBridge::new().ok().map(|b| b.script)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::OpenOptions;
    use std::sync::Mutex;

    struct FakeItem {
        entry: String,
        path: String,
        received: i64,
        flagged: bool,
        unread: bool,
        msg: Vec<u8>,
    }

    struct FakeOutlook {
        items: Mutex<Vec<FakeItem>>,
        deleted: Mutex<Vec<String>>,
        exports: Mutex<Vec<(Option<i64>, Option<i64>)>>,
    }

    impl Bridge for FakeOutlook {
        fn export(&self, req: &ExportRequest, tx: mpsc::SyncSender<BridgeLine>) -> Result<()> {
            self.exports.lock().unwrap().push((req.since, req.before));
            for (i, it) in self.items.lock().unwrap().iter().enumerate() {
                if req.since.is_some_and(|s| it.received < s) || req.before.is_some_and(|b| it.received >= b) {
                    continue;
                }
                let file = req.out_dir.join(format!("{i}.msg"));
                std::fs::write(&file, &it.msg)?;
                tx.send(BridgeLine::Item {
                    file,
                    entry_id: it.entry.clone(),
                    store_id: "S1".into(),
                    store: "alex@example.com".into(),
                    path: it.path.clone(),
                    received: it.received,
                    flagged: it.flagged,
                    unread: it.unread,
                })?;
            }
            Ok(())
        }
        fn delete(&self, items: &[(String, String)]) -> Result<(u64, u64)> {
            let mut all = self.items.lock().unwrap();
            let mut d = self.deleted.lock().unwrap();
            for (e, _) in items {
                all.retain(|i| &i.entry != e);
                d.push(e.clone());
            }
            Ok((items.len() as u64, 0))
        }
        fn list(&self) -> Result<Vec<BridgeLine>> {
            Ok(vec![])
        }
    }

    /// A .msg whose subject makes it unique.
    fn msg_with(subject: &str) -> Vec<u8> {
        let base = crate::import::msg::tests::build_msg();
        // Rebuild with a different subject stream.
        let mut cf = cfb::CompoundFile::open(std::io::Cursor::new(base)).unwrap();
        {
            use std::io::Write;
            let mut s = cf.create_stream("/__substg1.0_0037001F").unwrap();
            let u: Vec<u8> = subject.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
            s.write_all(&u).unwrap();
        }
        cf.flush().unwrap();
        cf.into_inner().into_inner()
    }

    #[test]
    fn archive_then_delete_only_safe_items() {
        let now = chrono::Utc::now().timestamp();
        let day = 86_400;
        let fake = FakeOutlook {
            items: Mutex::new(vec![
                FakeItem { entry: "old-read".into(), path: "Inbox".into(), received: now - 400 * day, flagged: false, unread: false, msg: msg_with("Old read") },
                FakeItem { entry: "old-flagged".into(), path: "Inbox".into(), received: now - 400 * day, flagged: true, unread: false, msg: msg_with("Old flagged") },
                FakeItem { entry: "old-unread".into(), path: "Inbox/Receipts".into(), received: now - 400 * day, flagged: false, unread: true, msg: msg_with("Old unread") },
                FakeItem { entry: "new".into(), path: "Inbox".into(), received: now - 2 * day, flagged: false, unread: false, msg: msg_with("New") },
            ]),
            deleted: Mutex::new(vec![]),
            exports: Mutex::new(vec![]),
        };
        let d = tempfile::tempdir().unwrap();
        let mut a = Archive::open(&d.path().join("a"), &OpenOptions { cache_dir: Some(d.path().join("c")), writer: true, create: true, ..Default::default() }).unwrap();
        let cfg = OutlookConfig { delete: Some(DeleteRule { older_than_days: 365, include_unread: false }), ..Default::default() };
        let st = import_with(&mut a, &cfg, &fake, |_, _| {}).unwrap();
        assert_eq!(st.added, 4, "{st:?}");
        assert_eq!(st.deleted, 1);
        assert_eq!(*fake.deleted.lock().unwrap(), vec!["old-read".to_string()], "flagged, unread and recent mail kept");
        assert_eq!(a.index.search("source:\"outlook:alex@example.com\"", None, 0, 10).unwrap().total, 4);
        assert_eq!(a.index.search("folder:receipts", None, 0, 10).unwrap().total, 1);

        // Second run only asks Outlook for mail since the newest archived item.
        let st2 = import_with(&mut a, &cfg, &fake, |_, _| {}).unwrap();
        assert_eq!(st2.added, 0);
        let exports = fake.exports.lock().unwrap();
        assert!(exports[1].0.unwrap() >= now - 2 * day - 1, "incremental: {exports:?}");
    }

    #[test]
    fn date_window_limits_what_is_archived() {
        let now = chrono::Utc::now().timestamp();
        let fake = FakeOutlook {
            items: Mutex::new(vec![
                FakeItem { entry: "a".into(), path: "Inbox".into(), received: now - 100 * 86_400, flagged: false, unread: false, msg: msg_with("A") },
                FakeItem { entry: "b".into(), path: "Inbox".into(), received: now - 86_400, flagged: false, unread: false, msg: msg_with("B") },
            ]),
            deleted: Mutex::new(vec![]),
            exports: Mutex::new(vec![]),
        };
        let d = tempfile::tempdir().unwrap();
        let mut a = Archive::open(&d.path().join("a"), &OpenOptions { cache_dir: Some(d.path().join("c")), writer: true, create: true, ..Default::default() }).unwrap();
        let cfg = OutlookConfig { window: DateWindow { older_than_days: Some(30), ..Default::default() }, ..Default::default() };
        let st = import_with(&mut a, &cfg, &fake, |_, _| {}).unwrap();
        assert_eq!(st.added, 1);
        assert!(fake.deleted.lock().unwrap().is_empty(), "no delete rule, nothing deleted");
    }

    #[test]
    fn bridge_lines_parse() {
        let l: BridgeLine = serde_json::from_str(r#"{"type":"item","file":"C:\\t\\1.msg","entryId":"E","storeId":"S","store":"x","path":"Inbox","received":5,"flagged":false,"unread":true}"#).unwrap();
        assert!(matches!(l, BridgeLine::Item { unread: true, .. }));
        let l: BridgeLine = serde_json::from_str(r#"{"type":"done","deleted":3,"kept":1}"#).unwrap();
        assert_eq!(l, BridgeLine::Done { count: None, deleted: Some(3), kept: Some(1) });
    }
}
