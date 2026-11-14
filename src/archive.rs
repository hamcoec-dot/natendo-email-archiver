//! The archive: a portable folder (source of truth) plus a local cache
//! (SQLite catalog + Tantivy index) derived from it.
//!
//! ```text
//! <archive>/                 can live on local disk, USB, or a NAS share
//!   archive.json             identity + format version
//!   messages/ab/cd/<sha256>.eml   raw messages, immutable, content-addressed
//!   journal/000001.jsonl     append-only catalog (locations, sync state)
//!   writer.lock              present while a writer is active
//!
//! <cache>/                   local disk (default: OS cache dir / <archive id>)
//!   catalog.sqlite           metadata, facets helpers, contacts
//!   index/                   Tantivy full-text index
//! ```

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::catalog::Catalog;
use crate::index::{SearchIndex, INDEX_VERSION};
use crate::journal::{self, Entry, JournalWriter};
use crate::lock::WriterLock;
use crate::parse::{self, ParsedMessage};

pub const FORMAT_VERSION: u32 = 1;
/// Commit to disk every N new messages during an import.
const COMMIT_EVERY: usize = 2000;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub id: String,
    pub created: i64,
    pub app: String,
}

#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Override where the local cache (catalog + index) lives.
    pub cache_dir: Option<PathBuf>,
    /// Keep the cache inside the archive folder (portable USB use).
    pub cache_in_archive: bool,
    /// Take the single-writer lock (needed to import).
    pub writer: bool,
    pub break_lock: bool,
    /// Create the archive if the folder has none.
    pub create: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    New,
    /// Already archived; a new location was recorded.
    NewLocation,
    /// Already archived at this location; nothing changed.
    Duplicate,
}

/// A message hashed and parsed, ready to add. Built off the main thread.
pub struct Prepared {
    pub id: String,
    pub raw: Vec<u8>,
    /// None when the catalog already holds this id (no need to parse).
    pub parsed: Option<ParsedMessage>,
    pub src: String,
    pub folders: Vec<String>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ImportStats {
    pub added: u64,
    pub new_locations: u64,
    pub duplicates: u64,
    pub failed: u64,
    pub errors: Vec<String>,
}

impl ImportStats {
    pub fn record(&mut self, o: AddOutcome) {
        match o {
            AddOutcome::New => self.added += 1,
            AddOutcome::NewLocation => self.new_locations += 1,
            AddOutcome::Duplicate => self.duplicates += 1,
        }
    }
    pub fn fail(&mut self, what: String) {
        self.failed += 1;
        if self.errors.len() < 200 {
            self.errors.push(what);
        }
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct VerifyReport {
    pub checked: u64,
    pub ok: u64,
    pub missing: Vec<String>,
    pub corrupt: Vec<String>,
    pub unknown_files: u64,
}

pub struct Archive {
    pub root: PathBuf,
    pub cache: PathBuf,
    pub manifest: Manifest,
    pub catalog: Catalog,
    pub index: SearchIndex,
    lock: Option<WriterLock>,
    journal: Option<JournalWriter>,
    dirty: HashSet<String>,
    pending: usize,
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn default_cache_root() -> PathBuf {
    if let Ok(d) = std::env::var("NEA_CACHE_DIR") {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).ok();
    #[cfg(target_os = "macos")]
    let base = std::env::var("HOME").ok().map(|h| PathBuf::from(h).join("Library/Caches"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".cache")));
    base.unwrap_or_else(std::env::temp_dir).join("NatendoEmailArchiver")
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_data()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

impl Archive {
    pub fn open(root: &Path, opts: &OpenOptions) -> Result<Self> {
        let manifest_path = root.join("archive.json");
        if !manifest_path.exists() {
            if !opts.create {
                bail!("{} is not an archive (no archive.json). Use `nea init` to create one.", root.display());
            }
            fs::create_dir_all(root.join("messages"))?;
            fs::create_dir_all(root.join("journal"))?;
            let seed = format!("{}-{}-{:?}", root.display(), std::process::id(), std::time::SystemTime::now());
            let m = Manifest {
                format: FORMAT_VERSION,
                id: sha256_hex(seed.as_bytes())[..16].to_string(),
                created: chrono::Utc::now().timestamp(),
                app: format!("Natendo Email Archiver {}", env!("CARGO_PKG_VERSION")),
            };
            write_atomic(&manifest_path, serde_json::to_string_pretty(&m)?.as_bytes())?;
        }
        let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        if manifest.format > FORMAT_VERSION {
            bail!("archive format {} is newer than this app supports ({FORMAT_VERSION}); please update", manifest.format);
        }
        let cache = match (&opts.cache_dir, opts.cache_in_archive) {
            (Some(d), _) => d.clone(),
            (None, true) => root.join("cache").join(crate::lock::hostname()),
            (None, false) => default_cache_root().join(&manifest.id),
        };
        fs::create_dir_all(&cache)?;
        fs::write(cache.join("archive-path.txt"), root.display().to_string()).ok();

        let lock = if opts.writer { Some(WriterLock::acquire(root, opts.break_lock)?) } else { None };
        let journal = if opts.writer { Some(JournalWriter::open(root)?) } else { None };

        let catalog = Catalog::open(&cache.join("catalog.sqlite"))?;
        let index = SearchIndex::open(&cache.join("index"))?;
        let mut a = Archive {
            root: root.to_path_buf(),
            cache,
            manifest,
            catalog,
            index,
            lock,
            journal,
            dirty: HashSet::new(),
            pending: 0,
        };
        let ver: Option<String> = a
            .catalog
            .conn
            .query_row("SELECT v FROM meta WHERE k='index_version'", [], |r| r.get(0))
            .ok();
        if ver.as_deref() != Some(INDEX_VERSION) && a.catalog.message_count()? > 0 {
            eprintln!("Search index format changed; rebuilding…");
            a.rebuild_index(|_, _| {})?;
        }
        a.catalog.conn.execute(
            "INSERT INTO meta (k,v) VALUES ('index_version',?1) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            [INDEX_VERSION],
        )?;
        a.try_sync();
        Ok(a)
    }

    /// Sync from the journal, tolerating a busy cache (another local process
    /// mid-import). Returns entries applied.
    pub fn try_sync(&mut self) -> u64 {
        match self.sync_from_journal() {
            Ok(n) => n,
            Err(e) => {
                if !self.catalog.conn.is_autocommit() {
                    let _ = self.catalog.conn.execute_batch("ROLLBACK");
                }
                self.dirty.clear();
                eprintln!("note: journal sync deferred: {e:#}");
                0
            }
        }
    }

    pub fn is_writer(&self) -> bool {
        self.lock.is_some()
    }

    pub fn message_path(&self, id: &str) -> PathBuf {
        message_path(&self.root, id)
    }

    pub fn read_raw(&self, id: &str) -> Result<Vec<u8>> {
        if id.len() != 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid message id");
        }
        let p = self.message_path(id);
        fs::read(&p).with_context(|| format!("reading {}", p.display()))
    }

    /// Hash and (if new) parse a message. Pure; safe to call from any thread.
    pub fn prepare(raw: Vec<u8>, src: String, folders: Vec<String>, known: impl Fn(&str) -> bool) -> Prepared {
        let id = sha256_hex(&raw);
        let parsed = if known(&id) { None } else { Some(parse::parse(&raw)) };
        Prepared { id, raw, parsed, src, folders }
    }

    pub fn add(&mut self, raw: &[u8], src: &str, folders: &[String]) -> Result<AddOutcome> {
        let p = Self::prepare(raw.to_vec(), src.to_string(), folders.to_vec(), |_| false);
        self.add_prepared(p)
    }

    pub fn add_prepared(&mut self, p: Prepared) -> Result<AddOutcome> {
        if self.journal.is_none() {
            bail!("archive is open read-only");
        }
        if self.catalog.conn.is_autocommit() {
            self.catalog.begin()?;
        }
        let now = chrono::Utc::now().timestamp();
        let folders: Vec<String> = if p.folders.is_empty() { vec![String::new()] } else { p.folders };
        let exists = self.catalog.has_message(&p.id)?;
        let mut outcome = if exists { AddOutcome::Duplicate } else { AddOutcome::New };
        if !exists {
            let path = self.message_path(&p.id);
            if !path.exists() {
                fs::create_dir_all(path.parent().unwrap())?;
                write_atomic(&path, &p.raw)?;
            }
            let parsed = match p.parsed {
                Some(x) => x,
                None => parse::parse(&p.raw),
            };
            self.catalog.insert_message(&p.id, p.raw.len(), &parsed)?;
            let mut locs = Vec::new();
            for f in &folders {
                self.catalog.add_location(&p.id, &p.src, f, now)?;
                locs.push((p.src.clone(), f.clone()));
            }
            let doc = self.index.build_doc(&p.id, p.raw.len(), &parsed, &locs);
            self.index.upsert(doc, &p.id, false)?;
            self.pending += 1;
        }
        let j = self.journal.as_mut().unwrap();
        for f in &folders {
            let new_loc = exists && self.catalog.add_location(&p.id, &p.src, f, now)?;
            if !exists || new_loc {
                j.append(&Entry::Msg { id: p.id.clone(), src: p.src.clone(), folder: f.clone(), at: now })?;
            }
            if new_loc {
                outcome = AddOutcome::NewLocation;
                self.dirty.insert(p.id.clone());
            }
        }
        if self.pending + self.dirty.len() >= COMMIT_EVERY {
            self.commit()?;
        }
        if let Some(l) = self.lock.as_mut() {
            l.heartbeat();
        }
        Ok(outcome)
    }

    /// An archived message with this Message-ID and exact size, if any.
    pub fn find_by_msgid_size(&self, message_id: &str, size: i64) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .catalog
            .conn
            .query_row("SELECT id FROM messages WHERE message_id=?1 AND size=?2 LIMIT 1", rusqlite::params![message_id, size], |r| r.get(0))
            .optional()?)
    }

    /// Record another location for a message that is already archived.
    pub fn add_location_existing(&mut self, id: &str, src: &str, folder: &str) -> Result<AddOutcome> {
        let j = self.journal.as_mut().context("archive is open read-only")?;
        if self.catalog.conn.is_autocommit() {
            self.catalog.begin()?;
        }
        let now = chrono::Utc::now().timestamp();
        if self.catalog.add_location(id, src, folder, now)? {
            j.append(&Entry::Msg { id: id.into(), src: src.into(), folder: folder.into(), at: now })?;
            self.dirty.insert(id.to_string());
            Ok(AddOutcome::NewLocation)
        } else {
            Ok(AddOutcome::Duplicate)
        }
    }

    pub fn set_sync_state(&mut self, src: &str, folder: &str, uidvalidity: u32, last_uid: u32) -> Result<()> {
        let j = self.journal.as_mut().context("archive is open read-only")?;
        j.append(&Entry::Sync { src: src.into(), folder: folder.into(), uidvalidity, last_uid })?;
        self.catalog.set_sync(src, folder, uidvalidity, last_uid)
    }

    pub fn record_import(&mut self, kind: &str, path: &str, s: &ImportStats) -> Result<()> {
        let at = chrono::Utc::now().timestamp();
        if let Some(j) = self.journal.as_mut() {
            j.append(&Entry::Import { kind: kind.into(), path: path.into(), at, added: s.added, dupes: s.duplicates, failed: s.failed })?;
        }
        self.catalog.record_import(kind, path, at, s.added, s.duplicates, s.failed)
    }

    /// Durably persist everything added so far: journal first (the source of
    /// truth), then the local cache.
    pub fn commit(&mut self) -> Result<()> {
        if let Some(j) = self.journal.as_mut() {
            j.flush()?;
        }
        let dirty: Vec<String> = self.dirty.drain().collect();
        for id in dirty {
            self.reindex(&id)?;
        }
        self.index.commit()?;
        self.catalog.commit()?;
        self.pending = 0;
        // Our own entries are already applied; advance the replay offsets.
        if self.journal.is_some() {
            for (seg, path) in journal::segments(&self.root)? {
                let len = fs::metadata(&path)?.len();
                self.catalog.set_applied_offset(seg, len)?;
            }
        }
        Ok(())
    }

    fn reindex(&mut self, id: &str) -> Result<()> {
        let raw = self.read_raw(id)?;
        let parsed = parse::parse(&raw);
        let locs = self.catalog.locations(id)?;
        let doc = self.index.build_doc(id, raw.len(), &parsed, &locs);
        self.index.upsert(doc, id, true)
    }

    /// Apply journal entries written since this cache last looked (by this
    /// machine or another one sharing the archive folder).
    pub fn sync_from_journal(&mut self) -> Result<u64> {
        let mut applied = 0u64;
        let mut touched = false;
        for (seg, path) in journal::segments(&self.root)? {
            let off = self.catalog.applied_offset(seg)?;
            let len = fs::metadata(&path)?.len();
            if len <= off {
                continue;
            }
            let (entries, new_off) = journal::read_from(&path, off)?;
            if entries.is_empty() {
                continue;
            }
            touched = true;
            self.catalog.begin()?;
            for e in entries {
                match e {
                    Entry::Msg { id, src, folder, at } => {
                        if !self.catalog.has_message(&id)? {
                            let raw = match self.read_raw(&id) {
                                Ok(r) => r,
                                Err(_) => continue, // file not yet visible (NAS lag); retried next sync
                            };
                            let parsed = parse::parse(&raw);
                            self.catalog.insert_message(&id, raw.len(), &parsed)?;
                            self.catalog.add_location(&id, &src, &folder, at)?;
                            self.dirty.insert(id);
                        } else if self.catalog.add_location(&id, &src, &folder, at)? {
                            self.dirty.insert(id);
                        }
                    }
                    Entry::Sync { src, folder, uidvalidity, last_uid } => {
                        self.catalog.set_sync(&src, &folder, uidvalidity, last_uid)?;
                    }
                    Entry::Import { kind, path, at, added, dupes, failed } => {
                        self.catalog.record_import(&kind, &path, at, added, dupes, failed)?;
                    }
                }
                applied += 1;
            }
            self.catalog.set_applied_offset(seg, new_off)?;
            self.catalog.commit()?;
        }
        if touched {
            // New-to-this-cache messages need a full index doc, which
            // reindex() builds from the file and catalog locations.
            let dirty: Vec<String> = self.dirty.drain().collect();
            for id in &dirty {
                self.reindex(id)?;
            }
            self.index.commit()?;
            if !self.is_writer() {
                self.index.close_writer()?;
            }
        }
        Ok(applied)
    }

    /// Drop and rebuild the search index from the message files.
    pub fn rebuild_index(&mut self, mut progress: impl FnMut(usize, usize)) -> Result<()> {
        self.index.clear()?;
        self.index.commit()?;
        let ids = self.catalog.all_ids()?;
        let total = ids.len();
        let (tx, rx) = mpsc::sync_channel::<(String, Vec<u8>, ParsedMessage)>(256);
        let root = self.root.clone();
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
        let chunks: Vec<Vec<String>> = {
            let mut c = vec![Vec::new(); workers];
            for (i, id) in ids.into_iter().enumerate() {
                c[i % workers].push(id);
            }
            c
        };
        std::thread::scope(|s| -> Result<()> {
            for chunk in chunks {
                let tx = tx.clone();
                let root = root.clone();
                s.spawn(move || {
                    for id in chunk {
                        if let Ok(raw) = fs::read(message_path(&root, &id)) {
                            let p = parse::parse(&raw);
                            if tx.send((id, raw, p)).is_err() {
                                break;
                            }
                        }
                    }
                });
            }
            drop(tx);
            let mut n = 0;
            for (id, raw, p) in rx {
                let locs = self.catalog.locations(&id)?;
                let doc = self.index.build_doc(&id, raw.len(), &p, &locs);
                self.index.upsert(doc, &id, false)?;
                n += 1;
                if n % 1000 == 0 {
                    progress(n, total);
                }
            }
            progress(n, total);
            Ok(())
        })?;
        self.index.commit()?;
        if !self.is_writer() {
            self.index.close_writer()?;
        }
        Ok(())
    }

    /// Re-hash every message file and compare with its name and the catalog.
    pub fn verify(&self, mut progress: impl FnMut(u64)) -> Result<VerifyReport> {
        let mut r = VerifyReport::default();
        let known: HashSet<String> = self.catalog.all_ids()?.into_iter().collect();
        let mut seen = HashSet::new();
        for e in walkdir::WalkDir::new(self.root.join("messages")).into_iter().filter_map(|e| e.ok()) {
            if !e.file_type().is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy();
            let Some(id) = name.strip_suffix(".eml") else {
                r.unknown_files += 1;
                continue;
            };
            r.checked += 1;
            let data = fs::read(e.path())?;
            if sha256_hex(&data) == id {
                r.ok += 1;
            } else {
                r.corrupt.push(id.to_string());
            }
            seen.insert(id.to_string());
            if r.checked % 1000 == 0 {
                progress(r.checked);
            }
        }
        r.missing = known.difference(&seen).cloned().collect();
        r.missing.sort();
        Ok(r)
    }
}

impl Drop for Archive {
    fn drop(&mut self) {
        if self.journal.is_some() {
            let _ = self.commit();
        }
    }
}

pub fn message_path(root: &Path, id: &str) -> PathBuf {
    root.join("messages").join(&id[..2]).join(&id[2..4]).join(format!("{id}.eml"))
}

/// Run `prepare` on worker threads and feed results to `archive.add_prepared`
/// on the calling thread. `items` yields (raw, src, folders) or an error label.
pub fn ingest<I>(archive: &mut Archive, items: I, stats: &mut ImportStats, mut progress: impl FnMut(&ImportStats)) -> Result<()>
where
    I: Iterator<Item = std::result::Result<(Vec<u8>, String, Vec<String>), String>> + Send,
{
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let catalog_path = archive.cache.join("catalog.sqlite");
    let (in_tx, in_rx) = crossbeam_like::channel(workers * 4);
    let (out_tx, out_rx) = mpsc::sync_channel::<Prepared>(workers * 8);
    let mut errors: Vec<String> = Vec::new();
    std::thread::scope(|s| -> Result<()> {
        for _ in 0..workers {
            let rx = in_rx.clone();
            let tx = out_tx.clone();
            let cp = catalog_path.clone();
            s.spawn(move || {
                // Each worker checks for known ids with its own read connection.
                let conn = rusqlite::Connection::open_with_flags(&cp, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok();
                let known = |id: &str| -> bool {
                    conn.as_ref()
                        .and_then(|c| c.query_row("SELECT 1 FROM messages WHERE id=?1", [id], |_| Ok(())).ok())
                        .is_some()
                };
                while let Some((raw, src, folders)) = rx.recv() {
                    if tx.send(Archive::prepare(raw, src, folders, &known)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(out_tx);
        // Feeder thread reads input so I/O overlaps parsing.
        let feeder_errors = s.spawn(move || {
            let mut errs = Vec::new();
            for item in items {
                match item {
                    Ok(x) => {
                        if !in_tx.send(x) {
                            break;
                        }
                    }
                    Err(e) => errs.push(e),
                }
            }
            drop(in_tx);
            errs
        });
        let mut n = 0u64;
        for p in out_rx {
            match archive.add_prepared(p) {
                Ok(o) => stats.record(o),
                Err(e) => stats.fail(format!("{e:#}")),
            }
            n += 1;
            if n % 250 == 0 {
                progress(stats);
            }
        }
        errors = feeder_errors.join().unwrap_or_default();
        Ok(())
    })?;
    for e in errors {
        stats.fail(e);
    }
    archive.commit()?;
    progress(stats);
    Ok(())
}

/// Minimal multi-consumer bounded channel (std mpsc is single-consumer).
mod crossbeam_like {
    use std::collections::VecDeque;
    use std::sync::{Arc, Condvar, Mutex};

    struct Inner<T> {
        q: Mutex<(VecDeque<T>, bool)>, // (queue, closed)
        not_empty: Condvar,
        not_full: Condvar,
        cap: usize,
    }

    pub struct Sender<T>(Arc<Inner<T>>);
    pub struct Receiver<T>(Arc<Inner<T>>);

    impl<T> Clone for Receiver<T> {
        fn clone(&self) -> Self {
            Receiver(self.0.clone())
        }
    }

    pub fn channel<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
        let i = Arc::new(Inner { q: Mutex::new((VecDeque::new(), false)), not_empty: Condvar::new(), not_full: Condvar::new(), cap: cap.max(1) });
        (Sender(i.clone()), Receiver(i))
    }

    impl<T> Sender<T> {
        /// Returns false if all receivers are gone (never, here) — kept for symmetry.
        pub fn send(&self, v: T) -> bool {
            let mut g = self.0.q.lock().unwrap();
            while g.0.len() >= self.0.cap {
                g = self.0.not_full.wait(g).unwrap();
            }
            g.0.push_back(v);
            self.0.not_empty.notify_one();
            true
        }
    }

    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            let mut g = self.0.q.lock().unwrap();
            g.1 = true;
            self.0.not_empty.notify_all();
        }
    }

    impl<T> Receiver<T> {
        pub fn recv(&self) -> Option<T> {
            let mut g = self.0.q.lock().unwrap();
            loop {
                if let Some(v) = g.0.pop_front() {
                    self.0.not_full.notify_one();
                    return Some(v);
                }
                if g.1 {
                    return None;
                }
                g = self.0.not_empty.wait(g).unwrap();
            }
        }
    }
}
