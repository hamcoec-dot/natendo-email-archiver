//! Local SQLite cache of archive metadata. Fully derived from the archive
//! folder (journal + message files), so it lives on local disk and can be
//! deleted at any time.

use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::parse::ParsedMessage;

pub struct Catalog {
    pub conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT);
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    date INTEGER NOT NULL,
    subject TEXT NOT NULL,
    from_addr TEXT NOT NULL,
    from_name TEXT NOT NULL,
    message_id TEXT NOT NULL,
    thread_key TEXT NOT NULL,
    att_count INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_date ON messages(date);
CREATE INDEX IF NOT EXISTS messages_thread ON messages(thread_key);
CREATE INDEX IF NOT EXISTS messages_from ON messages(from_addr);
CREATE INDEX IF NOT EXISTS messages_msgid ON messages(message_id);
CREATE TABLE IF NOT EXISTS locations (
    id TEXT NOT NULL, src TEXT NOT NULL, folder TEXT NOT NULL, at INTEGER NOT NULL,
    PRIMARY KEY (id, src, folder)
);
CREATE INDEX IF NOT EXISTS locations_folder ON locations(src, folder);
CREATE TABLE IF NOT EXISTS contacts (
    email TEXT PRIMARY KEY, name TEXT NOT NULL, count INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sync (
    src TEXT NOT NULL, folder TEXT NOT NULL, uidvalidity INTEGER NOT NULL, last_uid INTEGER NOT NULL,
    PRIMARY KEY (src, folder)
);
CREATE TABLE IF NOT EXISTS imports (
    kind TEXT, path TEXT, at INTEGER, added INTEGER, dupes INTEGER, failed INTEGER
);
CREATE TABLE IF NOT EXISTS applied (seg INTEGER PRIMARY KEY, off INTEGER NOT NULL);
"#;

#[derive(Debug, Clone, serde::Serialize)]
pub struct FolderCount {
    pub src: String,
    pub folder: String,
    pub count: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Stats {
    pub messages: i64,
    pub bytes: i64,
    pub attachments: i64,
    pub first_date: Option<i64>,
    pub last_date: Option<i64>,
    pub sources: Vec<String>,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }
    pub fn commit(&self) -> Result<()> {
        if !self.conn.is_autocommit() {
            self.conn.execute_batch("COMMIT")?;
        }
        Ok(())
    }

    pub fn has_message(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM messages WHERE id=?1", [id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn insert_message(&self, id: &str, size: usize, p: &ParsedMessage) -> Result<()> {
        let from = p.from.first().cloned().unwrap_or_default();
        self.conn.execute(
            "INSERT OR IGNORE INTO messages (id,size,date,subject,from_addr,from_name,message_id,thread_key,att_count)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id,
                size as i64,
                p.date,
                p.subject,
                from.email,
                from.name,
                p.message_id,
                p.thread_key(),
                p.attachments.iter().filter(|a| !a.inline).count() as i64
            ],
        )?;
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO contacts (email,name,count) VALUES (?1,?2,1)
             ON CONFLICT(email) DO UPDATE SET count=count+1,
               name=CASE WHEN excluded.name<>'' THEN excluded.name ELSE name END",
        )?;
        for a in p.from.iter().chain(&p.to).chain(&p.cc) {
            if !a.email.is_empty() {
                stmt.execute(params![a.email, a.name])?;
            }
        }
        Ok(())
    }

    /// Returns true if the location is new.
    pub fn add_location(&self, id: &str, src: &str, folder: &str, at: i64) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO locations (id,src,folder,at) VALUES (?1,?2,?3,?4)",
            params![id, src, folder, at],
        )?;
        Ok(n > 0)
    }

    pub fn locations(&self, id: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare_cached("SELECT src, folder FROM locations WHERE id=?1 ORDER BY src, folder")?;
        let rows = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn all_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT id FROM messages ORDER BY date")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn message_count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?)
    }

    pub fn applied_offset(&self, seg: u32) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT off FROM applied WHERE seg=?1", [seg], |r| r.get::<_, i64>(0))
            .optional()?
            .unwrap_or(0) as u64)
    }
    pub fn set_applied_offset(&self, seg: u32, off: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO applied (seg,off) VALUES (?1,?2) ON CONFLICT(seg) DO UPDATE SET off=excluded.off",
            params![seg, off as i64],
        )?;
        Ok(())
    }

    pub fn set_sync(&self, src: &str, folder: &str, uidvalidity: u32, last_uid: u32) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync (src,folder,uidvalidity,last_uid) VALUES (?1,?2,?3,?4)
             ON CONFLICT(src,folder) DO UPDATE SET uidvalidity=excluded.uidvalidity, last_uid=excluded.last_uid",
            params![src, folder, uidvalidity, last_uid],
        )?;
        Ok(())
    }
    pub fn get_sync(&self, src: &str, folder: &str) -> Result<Option<(u32, u32)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT uidvalidity,last_uid FROM sync WHERE src=?1 AND folder=?2",
                [src, folder],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn record_import(&self, kind: &str, path: &str, at: i64, added: u64, dupes: u64, failed: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO imports (kind,path,at,added,dupes,failed) VALUES (?1,?2,?3,?4,?5,?6)",
            params![kind, path, at, added as i64, dupes as i64, failed as i64],
        )?;
        Ok(())
    }

    pub fn imports(&self) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare("SELECT kind,path,at,added,dupes,failed FROM imports ORDER BY at DESC LIMIT 50")?;
        let rows = stmt.query_map([], |r| {
            Ok(serde_json::json!({
                "kind": r.get::<_, String>(0)?, "path": r.get::<_, String>(1)?, "at": r.get::<_, i64>(2)?,
                "added": r.get::<_, i64>(3)?, "dupes": r.get::<_, i64>(4)?, "failed": r.get::<_, i64>(5)?,
            }))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn folders(&self) -> Result<Vec<FolderCount>> {
        let mut stmt = self.conn.prepare(
            "SELECT src, folder, COUNT(*) FROM locations GROUP BY src, folder ORDER BY src, folder",
        )?;
        let rows = stmt.query_map([], |r| Ok(FolderCount { src: r.get(0)?, folder: r.get(1)?, count: r.get(2)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn contacts(&self, prefix: &str, limit: usize) -> Result<Vec<(String, String, i64)>> {
        let like = format!("%{}%", prefix.replace('%', "").replace('_', "").to_lowercase());
        let mut stmt = self.conn.prepare(
            "SELECT email, name, count FROM contacts
             WHERE email LIKE ?1 OR lower(name) LIKE ?1 ORDER BY count DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![like, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn stats(&self) -> Result<Stats> {
        let (messages, bytes, attachments, first, last) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size),0), COALESCE(SUM(att_count),0),
                    MIN(NULLIF(date,0)), MAX(NULLIF(date,0)) FROM messages",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let mut stmt = self.conn.prepare("SELECT DISTINCT src FROM locations ORDER BY src")?;
        let sources = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(Stats { messages, bytes, attachments, first_date: first, last_date: last, sources })
    }

    pub fn thread_ids(&self, thread_key: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT id FROM messages WHERE thread_key=?1 ORDER BY date")?;
        let rows = stmt.query_map([thread_key], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
