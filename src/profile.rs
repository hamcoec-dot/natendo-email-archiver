//! Saved archiving profiles: a source plus its rules, re-runnable any time
//! (from the UI, the command line, a desktop shortcut or a scheduled task).
//! Stored in the archive folder as `profiles.json` so they travel with it.
//! Passwords are never stored there.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::archive::{Archive, ImportStats};
use crate::import::imap::ImapConfig;
use crate::rules::{DateWindow, DeleteRule};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Imap(ImapConfig),
    Pop3(crate::import::pop3::Pop3Config),
    Outlook(crate::import::outlook::OutlookConfig),
    Mbox {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        src: Option<String>,
    },
    Eml {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        src: Option<String>,
    },
    Mailstore {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        src: Option<String>,
    },
    Pst {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        src: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub source: Source,
    /// Archive only messages in this window (file sources).
    #[serde(default)]
    pub window: DateWindow,
    /// Environment variable that holds the password (mail servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<i64>,
}

impl Profile {
    pub fn describe(&self) -> String {
        match &self.source {
            Source::Imap(c) => format!("IMAP {}@{}{}", c.user, c.host, rules_text(&c.window, c.delete.as_ref())),
            Source::Pop3(c) => format!("POP3 {}@{}{}", c.user, c.host, rules_text(&c.window, c.delete.as_ref())),
            Source::Outlook(c) => format!(
                "Outlook {}{}{}",
                if c.stores.is_empty() { "(all mailboxes)".to_string() } else { c.stores.join(", ") },
                if c.folders.is_empty() { String::new() } else { format!(" / {}", c.folders.join(", ")) },
                rules_text(&c.window, c.delete.as_ref())
            ),
            Source::Mbox { path, .. } => format!("MBOX {}{}", path.display(), rules_text(&self.window, None)),
            Source::Eml { path, .. } => format!("EML folder {}{}", path.display(), rules_text(&self.window, None)),
            Source::Mailstore { path, .. } => format!("MailStore export {}{}", path.display(), rules_text(&self.window, None)),
            Source::Pst { path, .. } => format!("Outlook data file {}{}", path.display(), rules_text(&self.window, None)),
        }
    }

    /// The date window that applies to this profile's source.
    pub fn source_window(&self) -> &DateWindow {
        match &self.source {
            Source::Imap(c) => &c.window,
            Source::Pop3(c) => &c.window,
            Source::Outlook(c) => &c.window,
            _ => &self.window,
        }
    }

    pub fn needs_password(&self) -> bool {
        matches!(self.source, Source::Imap(_) | Source::Pop3(_))
    }
}

pub fn rules_text(w: &DateWindow, d: Option<&DeleteRule>) -> String {
    let mut parts = Vec::new();
    if let Some(a) = &w.after {
        parts.push(format!("from {a}"));
    }
    if let Some(b) = &w.before {
        parts.push(format!("before {b}"));
    }
    if let Some(n) = w.older_than_days {
        parts.push(format!("older than {n} days"));
    }
    if let Some(d) = d {
        parts.push(if d.older_than_days > 0 {
            format!("then delete from source if older than {} days", d.older_than_days)
        } else {
            "then delete from source".to_string()
        });
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

fn path(root: &Path) -> PathBuf {
    root.join("profiles.json")
}

pub fn load(root: &Path) -> Result<Vec<Profile>> {
    let p = path(root);
    if !p.exists() {
        return Ok(vec![]);
    }
    serde_json::from_slice(&fs::read(&p)?).with_context(|| format!("reading {}", p.display()))
}

/// Save profiles. Only the archive writer may call this.
pub fn save(archive: &Archive, profiles: &[Profile]) -> Result<()> {
    if !archive.is_writer() {
        bail!("profiles can only be changed while holding the archive writer lock");
    }
    let p = path(&archive.root);
    let tmp = p.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(profiles)?)?;
    fs::rename(&tmp, &p)?;
    Ok(())
}

pub fn upsert(archive: &Archive, profile: Profile) -> Result<()> {
    let mut all = load(&archive.root)?;
    all.retain(|p| !p.name.eq_ignore_ascii_case(&profile.name));
    all.push(profile);
    all.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    save(archive, &all)
}

pub fn remove(archive: &Archive, name: &str) -> Result<bool> {
    let mut all = load(&archive.root)?;
    let before = all.len();
    all.retain(|p| !p.name.eq_ignore_ascii_case(name));
    save(archive, &all)?;
    Ok(all.len() != before)
}

pub fn find(root: &Path, name: &str) -> Result<Profile> {
    load(root)?
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .with_context(|| format!("no profile named {name:?}"))
}

/// Run a profile. `password` is asked for only when the source needs one.
pub fn run(
    archive: &mut Archive,
    profile: &Profile,
    password: impl FnOnce() -> Result<String>,
    mut progress: impl FnMut(&str, &ImportStats),
) -> Result<ImportStats> {
    let pw = if profile.needs_password() {
        match profile.password_env.as_deref().and_then(|v| std::env::var(v).ok()) {
            Some(p) => p,
            None => password()?,
        }
    } else {
        String::new()
    };
    archive.window = Some(profile.window.clone());
    let result = (|| -> Result<ImportStats> {
        let mut file_progress = |s: &ImportStats| progress(&profile.name, s);
        Ok(match &profile.source {
            Source::Imap(c) => {
                let mut c = c.clone();
                c.password = pw;
                archive.window = None; // IMAP applies its own window server-side
                crate::import::imap::import(archive, &c, |f, s| progress(f, s))?
            }
            Source::Pop3(c) => {
                let mut c = c.clone();
                c.password = pw;
                archive.window = Some(c.window.clone());
                crate::import::pop3::import(archive, &c, |s| progress(&profile.name, s))?
            }
            Source::Outlook(c) => {
                archive.window = None; // filtered inside Outlook
                crate::import::outlook::import(archive, c, |f, s| progress(f, s))?
            }
            Source::Mbox { path, src } => crate::import::mbox::import(archive, path, src.as_deref().unwrap_or("mbox"), &mut file_progress)?,
            Source::Eml { path, src } => {
                crate::import::eml::import(archive, path, src.as_deref().unwrap_or("files"), crate::import::eml::Flavor::Files, &mut file_progress)?
            }
            Source::Mailstore { path, src } => crate::import::eml::import(
                archive,
                path,
                src.as_deref().unwrap_or("mailstore"),
                crate::import::eml::Flavor::MailStore,
                &mut file_progress,
            )?,
            Source::Pst { path, src } => crate::import::pst::import(archive, path, src.as_deref().unwrap_or("outlook"), &mut file_progress)?,
        })
    })();
    archive.window = None;
    let stats = result?;
    let mut all = load(&archive.root)?;
    if let Some(p) = all.iter_mut().find(|p| p.name.eq_ignore_ascii_case(&profile.name)) {
        p.last_run = Some(chrono::Utc::now().timestamp());
        save(archive, &all)?;
    }
    Ok(stats)
}
