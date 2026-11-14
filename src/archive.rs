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
use crate::pack::{self, PackReader, PackWriter};
use crate::parse::{self, ParsedMessage};

pub const FORMAT_VERSION: u32 = 1;
/// During an import, commit (journal fsync + index commit) every N new
/// messages or every few seconds, whichever comes first. Commits are the
/// expensive part of indexing; a crash loses at most this much progress,
/// and re-running the import re-adds it.
const COMMIT_EVERY: usize = 25_000;
const COMMIT_SECS: u64 = 15;

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
    /// Outside the profile's date window, not archived.
    pub skipped: u64,
    /// Deleted from the source after archiving.
    pub deleted: u64,
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
    packw: Option<PackWriter>,
    pub packs: PackReader,
    dirty: HashSet<String>,
    pending: usize,
    /// When set, imports skip messages dated outside this window.
    pub window: Option<crate::rules::DateWindow>,
    last_commit: std::time::Instant,
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
        if std::env::var_os("NEA_NOSYNC").is_none() { f.sync_data()?; }
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
        let packw = if opts.writer { Some(PackWriter::open(root)?) } else { None };

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
            packw,
            packs: PackReader::new(root),
            dirty: HashSet::new(),
            pending: 0,
            window: None,
            last_commit: std::time::Instant::now(),
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

    pub fn read_raw(&self, id: &str) -> Result<Vec<u8>> {
        pack::check_id(id)?;
        let [n, off, len] = self.catalog.locate(id)?.with_context(|| format!("message {id} is not in the archive"))?;
        self.packs.read(n as u32, off, len)
    }

    /// True when the pack holding `loc` is long enough to contain it.
    fn pack_has(&self, loc: [u64; 3]) -> bool {
        fs::metadata(pack::pack_path(&self.root, loc[0] as u32)).map(|m| m.len() >= loc[1] + loc[2]).unwrap_or(false)
    }

    /// Pack file, offset and length of a message (for tools and tests).
    pub fn locate(&self, id: &str) -> Result<Option<(PathBuf, u64, u64)>> {
        Ok(self.catalog.locate(id)?.map(|[n, off, len]| (pack::pack_path(&self.root, n as u32), off, len)))
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
        let mut ploc = None;
        if !exists {
            let (n, off) = self.packw.as_mut().unwrap().append(&p.id, &p.raw)?;
            let loc = [n as u64, off, p.raw.len() as u64];
            ploc = Some(loc);
            let parsed = match p.parsed {
                Some(x) => x,
                None => parse::parse(&p.raw),
            };
            self.catalog.insert_message(&p.id, loc, &parsed)?;
            let mut locs = Vec::new();
            for f in &folders {
                self.catalog.add_location(&p.id, &p.src, f, now)?;
                locs.push((p.src.clone(), f.clone()));
            }
            let doc = self.index.build_doc(&p.id, p.raw.len(), &parsed, &locs, now);
            self.index.upsert(doc, &p.id, false)?;
            self.pending += 1;
        }
        let j = self.journal.as_mut().unwrap();
        for f in &folders {
            let new_loc = exists && self.catalog.add_location(&p.id, &p.src, f, now)?;
            if !exists || new_loc {
                // The first entry for a new message carries where its bytes are.
                j.append(&Entry::Msg { id: p.id.clone(), src: p.src.clone(), folder: f.clone(), at: now, p: ploc.take() })?;
            }
            if new_loc {
                outcome = AddOutcome::NewLocation;
                self.dirty.insert(p.id.clone());
            }
        }
        if self.pending + self.dirty.len() >= COMMIT_EVERY || self.last_commit.elapsed().as_secs() >= COMMIT_SECS {
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
            j.append(&Entry::Msg { id: id.into(), src: src.into(), folder: folder.into(), at: now, p: None })?;
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
        // Message bytes must be durable before the journal points at them.
        if let Some(w) = self.packw.as_mut() {
            w.sync()?;
        }
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
        self.last_commit = std::time::Instant::now();
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
        let doc = self.index.build_doc(id, raw.len(), &parsed, &locs, self.catalog.archived_at(id)?);
        self.index.upsert(doc, id, true)
    }

    /// Apply journal entries written since this cache last looked (by this
    /// machine or another one sharing the archive folder). The catalog
    /// commits only after the index does, so a failure leaves nothing half
    /// applied.
    pub fn sync_from_journal(&mut self) -> Result<u64> {
        let mut applied = 0u64;
        let mut offsets: Vec<(u32, u64)> = Vec::new();
        let mut began = false;
        let mut queued: std::collections::HashMap<String, [u64; 3]> = std::collections::HashMap::new();
        'segs: for (seg, path) in journal::segments(&self.root)? {
            let off = self.catalog.applied_offset(seg)?;
            let len = fs::metadata(&path)?.len();
            if len <= off {
                continue;
            }
            let (entries, _) = journal::read_from(&path, off)?;
            if entries.is_empty() {
                continue;
            }
            if !began {
                if self.catalog.conn.is_autocommit() {
                    self.catalog.begin()?;
                }
                began = true;
            }
            let mut done_to = off;
            for (e, end) in entries {
                match e {
                    Entry::Msg { id, src, folder, at, p } => {
                        if queued.contains_key(&id) {
                            self.catalog.add_location(&id, &src, &folder, at)?;
                        } else if !self.catalog.has_message(&id)? {
                            if let Some(loc) = p {
                                if !self.pack_has(loc) {
                                    // Bytes not visible yet (NAS caching lag):
                                    // stop here and retry from this entry later.
                                    offsets.push((seg, done_to));
                                    break 'segs;
                                }
                                self.catalog.add_location(&id, &src, &folder, at)?;
                                queued.insert(id, loc);
                            }
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
                done_to = end;
                applied += 1;
            }
            offsets.push((seg, done_to));
        }
        if !began {
            return Ok(0);
        }
        // New messages: parse once (in parallel) for catalog + index.
        // replace=true keeps this idempotent if an earlier attempt indexed
        // them but failed before the catalog committed.
        let new: Vec<(String, [u64; 3])> = queued.into_iter().collect();
        self.index_parallel(new, true, true, |_, _| {})?;
        let dirty: Vec<String> = self.dirty.drain().collect();
        self.reindex_many(&dirty)?;
        self.index.commit()?;
        if !self.is_writer() {
            self.index.close_writer()?;
        }
        for (seg, off) in offsets {
            self.catalog.set_applied_offset(seg, off)?;
        }
        self.catalog.commit()?;
        Ok(applied)
    }

    /// Re-index messages, parsing in parallel when there are many.
    fn reindex_many(&mut self, ids: &[String]) -> Result<()> {
        if ids.len() < 64 {
            for id in ids {
                self.reindex(id)?;
            }
            return Ok(());
        }
        let mut work = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(loc) = self.catalog.locate(id)? {
                work.push((id.clone(), loc));
            }
        }
        self.index_parallel(work, true, false, |_, _| {})
    }

    /// Parse messages on worker threads and index them on this thread.
    /// With `insert_catalog`, also creates their catalog rows (journal sync
    /// of messages new to this cache), so each message is parsed once.
    fn index_parallel(
        &mut self,
        work: Vec<(String, [u64; 3])>,
        replace: bool,
        insert_catalog: bool,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<()> {
        let total = work.len();
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
        let (tx, rx) = mpsc::sync_channel::<(String, [u64; 3], ParsedMessage)>(256);
        let mut chunks: Vec<Vec<(String, [u64; 3])>> = vec![Vec::new(); workers];
        for (i, w) in work.into_iter().enumerate() {
            chunks[i % workers].push(w);
        }
        let packs = self.packs.clone();
        std::thread::scope(|s| -> Result<()> {
            for chunk in chunks {
                let tx = tx.clone();
                let packs = packs.clone();
                s.spawn(move || {
                    for (id, loc) in chunk {
                        if let Ok(raw) = packs.read(loc[0] as u32, loc[1], loc[2]) {
                            let p = parse::parse(&raw);
                            if tx.send((id, loc, p)).is_err() {
                                break;
                            }
                        }
                    }
                });
            }
            drop(tx);
            let mut n = 0;
            for (id, loc, p) in rx {
                if insert_catalog {
                    self.catalog.insert_message(&id, loc, &p)?;
                }
                let locs = self.catalog.locations(&id)?;
                let doc = self.index.build_doc(&id, loc[2] as usize, &p, &locs, self.catalog.archived_at(&id)?);
                self.index.upsert(doc, &id, replace)?;
                n += 1;
                if n % 1000 == 0 {
                    progress(n, total);
                }
            }
            progress(n, total);
            Ok(())
        })
    }

    /// Drop and rebuild the search index from the message packs.
    pub fn rebuild_index(&mut self, progress: impl FnMut(usize, usize)) -> Result<()> {
        self.index.clear()?;
        self.index.commit()?;
        let work = self.catalog.all_locations()?;
        self.index_parallel(work, false, false, progress)?;
        self.index.commit()?;
        if !self.is_writer() {
            self.index.close_writer()?;
        }
        Ok(())
    }

    /// Re-hash every archived message, and scan packs for records the
    /// catalog does not know about.
    pub fn verify(&self, mut progress: impl FnMut(u64)) -> Result<VerifyReport> {
        let mut r = VerifyReport::default();
        for (id, [n, off, len]) in self.catalog.all_locations()? {
            r.checked += 1;
            match self.packs.read(n as u32, off, len) {
                Ok(data) if sha256_hex(&data) == id => r.ok += 1,
                Ok(_) => r.corrupt.push(id),
                Err(_) => r.missing.push(id),
            }
            if r.checked % 1000 == 0 {
                progress(r.checked);
            }
        }
        let known: HashSet<String> = self.catalog.all_ids()?.into_iter().collect();
        for (_, path) in pack::list_packs(&self.root)? {
            for (id, _, _) in pack::scan(&path)? {
                if !known.contains(&id) {
                    r.unknown_files += 1;
                }
            }
        }
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
        let window = archive.window.clone().filter(|w| !w.is_empty());
        for p in out_rx {
            if let (Some(w), Some(parsed)) = (&window, &p.parsed) {
                if !w.contains(parsed.date) {
                    stats.skipped += 1;
                    continue;
                }
            }
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
