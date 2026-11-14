//! Find mail stored on this computer: Thunderbird and SeaMonkey profiles,
//! Windows Live Mail / Windows Mail stores, and Outlook data files.
//! Only looks in the usual places; never walks the whole disk.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Found {
    /// Import kind: "mbox", "eml" or "pst" (or "outlook" for the live profile).
    pub kind: String,
    /// What it is, for people.
    pub label: String,
    pub path: PathBuf,
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_dir())
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    v.sort();
    v
}

fn files_with_ext(dir: &Path, ext: &[&str]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()).is_some_and(|e| ext.iter().any(|x| e.eq_ignore_ascii_case(x))))
        .collect();
    v.sort();
    v
}

/// Mozilla-style profiles root: each profile with mail gets one entry.
fn mozilla(root: &Path, app: &str, out: &mut Vec<Found>) {
    for prof in subdirs(root) {
        let has_mail = ["Mail", "ImapMail"].iter().any(|m| prof.join(m).is_dir());
        if has_mail {
            let name = prof.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let name = name.split_once('.').map(|(_, n)| n.to_string()).unwrap_or(name);
            out.push(Found { kind: "mbox".into(), label: format!("{app} profile \"{name}\""), path: prof });
        }
    }
}

/// Search the usual places under the given home / app-data folders.
pub fn detect_in(home: Option<&Path>, appdata: Option<&Path>, localappdata: Option<&Path>) -> Vec<Found> {
    let mut out = Vec::new();
    if let Some(ad) = appdata {
        mozilla(&ad.join("Thunderbird").join("Profiles"), "Thunderbird", &mut out);
        mozilla(&ad.join("Mozilla").join("SeaMonkey").join("Profiles"), "SeaMonkey", &mut out);
    }
    if let Some(h) = home {
        mozilla(&h.join(".thunderbird"), "Thunderbird", &mut out);
        mozilla(&h.join("snap").join("thunderbird").join("common").join(".thunderbird"), "Thunderbird", &mut out);
        mozilla(&h.join(".var").join("app").join("org.mozilla.Thunderbird").join(".thunderbird"), "Thunderbird", &mut out);
        mozilla(&h.join("Library").join("Thunderbird").join("Profiles"), "Thunderbird", &mut out);
        mozilla(&h.join(".mozilla").join("seamonkey"), "SeaMonkey", &mut out);
        for dir in [h.join("Documents").join("Outlook Files"), h.join("My Documents").join("Outlook Files")] {
            for f in files_with_ext(&dir, &["pst"]) {
                out.push(Found { kind: "pst".into(), label: format!("Outlook data file {}", f.file_name().unwrap().to_string_lossy()), path: f });
            }
        }
    }
    if let Some(la) = localappdata {
        for (dir, app) in [
            (la.join("Microsoft").join("Windows Live Mail"), "Windows Live Mail"),
            (la.join("Microsoft").join("Windows Mail"), "Windows Mail"),
        ] {
            if dir.is_dir() {
                out.push(Found { kind: "eml".into(), label: format!("{app} store"), path: dir });
            }
        }
        for f in files_with_ext(&la.join("Microsoft").join("Outlook"), &["pst"]) {
            out.push(Found { kind: "pst".into(), label: format!("Outlook data file {}", f.file_name().unwrap().to_string_lossy()), path: f });
        }
    }
    out.dedup_by(|a, b| a.path == b.path);
    out
}

/// Mail sources on this computer, plus the live Outlook profile on Windows.
pub fn detect() -> Vec<Found> {
    let home = env_path("USERPROFILE").or_else(|| env_path("HOME"));
    let mut out = detect_in(home.as_deref(), env_path("APPDATA").as_deref(), env_path("LOCALAPPDATA").as_deref());
    if cfg!(windows) && outlook_installed() {
        out.insert(0, Found { kind: "outlook".into(), label: "Outlook on this PC (all mailboxes)".into(), path: PathBuf::new() });
    }
    out
}

#[cfg(windows)]
fn outlook_installed() -> bool {
    // Classic Outlook registers its COM class here.
    std::process::Command::new("reg")
        .args(["query", r"HKCR\Outlook.Application\CLSID"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn outlook_installed() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_profiles_and_stores() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let ad = d.path().join("ad");
        let la = d.path().join("la");
        std::fs::create_dir_all(ad.join("Thunderbird/Profiles/abcd.default-release/Mail/Local Folders")).unwrap();
        std::fs::create_dir_all(ad.join("Thunderbird/Profiles/empty.other")).unwrap();
        std::fs::create_dir_all(home.join(".thunderbird/x1.default/ImapMail/imap.example.com")).unwrap();
        std::fs::create_dir_all(la.join("Microsoft/Windows Live Mail/Inbox")).unwrap();
        std::fs::create_dir_all(home.join("Documents/Outlook Files")).unwrap();
        std::fs::write(home.join("Documents/Outlook Files/Old.pst"), b"!BDN").unwrap();
        std::fs::write(home.join("Documents/Outlook Files/notes.txt"), b"x").unwrap();
        let f = detect_in(Some(&home), Some(&ad), Some(&la));
        let labels: Vec<&str> = f.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Thunderbird profile \"default-release\"",
                "Thunderbird profile \"default\"",
                "Outlook data file Old.pst",
                "Windows Live Mail store"
            ]
        );
        assert_eq!(f[3].kind, "eml");
        assert_eq!(f[2].kind, "pst");
    }
}
