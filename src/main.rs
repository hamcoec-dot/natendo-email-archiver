use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

use nea::archive::{Archive, ImportStats, OpenOptions};
use nea::import::{imap, mbox, outlook, pop3};
use nea::profile::{Profile, Source};
use nea::rules::{DateWindow, DeleteRule};
use nea::query::Sort;

/// Natendo Email Archiver: archive email locally and find anything in it.
#[derive(Parser)]
#[command(name = "nea", version, about)]
struct Cli {
    /// Archive folder (local disk, USB drive or network share).
    /// Default: "Natendo Archive" in your Documents folder.
    #[arg(short, long, env = "NEA_ARCHIVE", default_value_os_t = default_archive(), global = true)]
    archive: PathBuf,
    /// Where to keep the local search cache (default: OS cache folder).
    #[arg(long, env = "NEA_CACHE_DIR", global = true)]
    cache_dir: Option<PathBuf>,
    /// Keep the search cache inside the archive folder (portable USB use).
    #[arg(long, global = true)]
    cache_in_archive: bool,
    /// Take over the writer lock left by a crashed process.
    #[arg(long, global = true)]
    break_lock: bool,
    #[command(subcommand)]
    /// With no command, opens the web UI (double-click friendly).
    cmd: Option<Cmd>,
}

fn default_archive() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    match home {
        Some(h) if h.join("Documents").is_dir() => h.join("Documents").join("Natendo Archive"),
        Some(h) => h.join("Natendo Archive"),
        None => PathBuf::from("Natendo Archive"),
    }
}

#[derive(Subcommand, Clone)]
enum Cmd {
    /// Create a new archive folder.
    Init,
    /// Import mail into the archive.
    #[command(subcommand)]
    Import(ImportCmd),
    /// Saved archiving profiles (re-runnable imports with rules).
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// List mailboxes and mail folders in this PC's Outlook profile.
    OutlookFolders,
    /// Find mail on this computer (Outlook, Thunderbird, SeaMonkey, Windows Live Mail).
    Detect,
    /// Search the archive.
    Search {
        query: Vec<String>,
        #[arg(long, value_enum)]
        sort: Option<SortArg>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Print one message (headers and text).
    Show { id: String },
    /// Export messages matching a query (empty query = everything).
    Export {
        query: Vec<String>,
        #[arg(long, value_enum, default_value_t = Format::Eml)]
        format: Format,
        /// Output folder (eml) or file (mbox).
        #[arg(long)]
        out: PathBuf,
    },
    /// Copy messages matching a query into an IMAP mailbox (Gmail, Outlook.com, …).
    /// Folders are recreated; messages already there are skipped.
    ExportImap {
        query: Vec<String>,
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 993)]
        port: u16,
        #[arg(long)]
        user: String,
        #[arg(long, default_value = "NEA_IMAP_PASSWORD")]
        password_env: String,
        /// Put everything into this one folder.
        #[arg(long)]
        folder: Option<String>,
        /// Recreate archived folders under this folder (e.g. "Restored").
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        plaintext: bool,
    },
    /// Send messages matching a query, unchanged, to an email address (SMTP).
    ExportEmail {
        query: Vec<String>,
        /// Recipient (repeatable).
        #[arg(long, required = true)]
        to: Vec<String>,
        /// Envelope sender; usually your own address.
        #[arg(long)]
        from: String,
        #[arg(long)]
        host: String,
        /// 587 = STARTTLS, 465 = TLS.
        #[arg(long, default_value_t = 587)]
        port: u16,
        /// Login user (omit for relays without login).
        #[arg(long)]
        user: Option<String>,
        #[arg(long, default_value = "NEA_SMTP_PASSWORD")]
        password_env: String,
        /// Unencrypted (local test servers only).
        #[arg(long)]
        plaintext: bool,
    },
    /// Remove messages matching a query from the archive (asks first).
    Delete {
        query: Vec<String>,
        /// Don't ask.
        #[arg(long)]
        yes: bool,
    },
    /// Reclaim space from deleted messages by rewriting message packs.
    Compact,
    /// Back up the archive folder to another folder (incremental after the first run).
    Backup { dest: PathBuf },
    /// Re-hash every message file and report missing or corrupt ones.
    Verify,
    /// Rebuild the local search index from the archive folder.
    Rebuild,
    /// Archive statistics.
    Stats,
    /// Run the local web UI.
    Serve {
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// Open the browser.
        #[arg(long)]
        open: bool,
    },
    /// Write synthetic demo mail to an mbox file (for trying the app).
    Demo {
        #[arg(long, default_value_t = 5000)]
        messages: u64,
        #[arg(long)]
        out: PathBuf,
    },
    /// Benchmark import and search on synthetic mail in a temporary archive.
    Bench {
        #[arg(long, default_value_t = 20000)]
        messages: u64,
    },
}

/// Which messages to archive.
#[derive(Args, Clone, Default)]
struct WindowArgs {
    /// Only mail on or after this date (YYYY-MM-DD).
    #[arg(long)]
    after: Option<String>,
    /// Only mail before this date (YYYY-MM-DD).
    #[arg(long)]
    before: Option<String>,
    /// Only mail at least this many days old.
    #[arg(long, value_name = "DAYS")]
    older_than: Option<u32>,
}

impl WindowArgs {
    fn window(&self) -> DateWindow {
        DateWindow { after: self.after.clone(), before: self.before.clone(), older_than_days: self.older_than }
    }
}

/// Delete mail from the source once it is safely archived.
#[derive(Args, Clone, Default)]
struct DeleteArgs {
    /// After archiving, delete mail older than DAYS from the source
    /// (0 = any age). Flagged mail is never deleted; unread mail is kept
    /// unless --include-unread. Only mail the archive holds is deleted.
    #[arg(long, value_name = "DAYS")]
    delete_older_than: Option<u32>,
    /// With --delete-older-than, also delete unread mail.
    #[arg(long, requires = "delete_older_than")]
    include_unread: bool,
}

impl DeleteArgs {
    fn rule(&self) -> Option<DeleteRule> {
        self.delete_older_than.map(|d| DeleteRule { older_than_days: d, include_unread: self.include_unread })
    }
}

#[derive(Subcommand, Clone)]
enum ImportCmd {
    /// A folder of .eml files (subfolders become folders).
    Eml {
        dir: PathBuf,
        #[arg(long, default_value = "files")]
        src: String,
        #[command(flatten)]
        window: WindowArgs,
    },
    /// A MailStore Home "Export to directory (EML)" folder.
    Mailstore {
        dir: PathBuf,
        #[arg(long, default_value = "mailstore")]
        src: String,
        #[command(flatten)]
        window: WindowArgs,
    },
    /// An mbox file, Google Takeout mbox, or Thunderbird profile folder.
    Mbox {
        path: PathBuf,
        #[arg(long, default_value = "mbox")]
        src: String,
        #[command(flatten)]
        window: WindowArgs,
    },
    /// An Outlook .pst or .ost file (experimental).
    Pst {
        path: PathBuf,
        #[arg(long, default_value = "outlook")]
        src: String,
        #[command(flatten)]
        window: WindowArgs,
    },
    /// Mail straight from the Outlook profile on this PC (Windows, classic Outlook).
    Outlook {
        /// Only these mailboxes / data files (repeatable; see `nea outlook-folders`).
        #[arg(long)]
        store: Vec<String>,
        /// Only these folders, e.g. "Inbox" or "Inbox/Receipts" (repeatable).
        #[arg(long)]
        folder: Vec<String>,
        /// Also archive Deleted Items and Junk.
        #[arg(long)]
        include_all: bool,
        /// Scan everything again instead of only mail received since the last run.
        #[arg(long)]
        full_rescan: bool,
        #[command(flatten)]
        window: WindowArgs,
        #[command(flatten)]
        delete: DeleteArgs,
    },
    /// An IMAP account (Gmail, Outlook.com, Fastmail, any IMAP server).
    Imap {
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 993)]
        port: u16,
        #[arg(long)]
        user: String,
        /// Environment variable holding the password / app password
        /// (prompted for when not set).
        #[arg(long, default_value = "NEA_IMAP_PASSWORD")]
        password_env: String,
        /// Only these folders (repeatable).
        #[arg(long)]
        folder: Vec<String>,
        /// Also archive Spam/Junk/Trash folders.
        #[arg(long)]
        include_all: bool,
        /// Source name to record (default user@host).
        #[arg(long)]
        src: Option<String>,
        /// Allow an unencrypted connection (testing only).
        #[arg(long)]
        plaintext: bool,
        #[command(flatten)]
        window: WindowArgs,
        #[command(flatten)]
        delete: DeleteArgs,
    },
    /// A POP3 mailbox.
    Pop3 {
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 995)]
        port: u16,
        #[arg(long)]
        user: String,
        /// Environment variable holding the password (prompted for when not set).
        #[arg(long, default_value = "NEA_POP3_PASSWORD")]
        password_env: String,
        /// Source name to record (default user@host).
        #[arg(long)]
        src: Option<String>,
        /// Allow an unencrypted connection (testing only).
        #[arg(long)]
        plaintext: bool,
        #[command(flatten)]
        window: WindowArgs,
        #[command(flatten)]
        delete: DeleteArgs,
    },
}

impl ImportCmd {
    /// The same import as a (possibly unsaved) archiving profile.
    fn to_profile(&self, name: &str) -> Profile {
        let (source, window, password_env) = match self.clone() {
            ImportCmd::Eml { dir, src, window } => (Source::Eml { path: abs(dir), src: Some(src) }, window.window(), None),
            ImportCmd::Mailstore { dir, src, window } => (Source::Mailstore { path: abs(dir), src: Some(src) }, window.window(), None),
            ImportCmd::Mbox { path, src, window } => (Source::Mbox { path: abs(path), src: Some(src) }, window.window(), None),
            ImportCmd::Pst { path, src, window } => (Source::Pst { path: abs(path), src: Some(src) }, window.window(), None),
            ImportCmd::Outlook { store, folder, include_all, full_rescan, window, delete } => (
                Source::Outlook(outlook::OutlookConfig {
                    stores: store,
                    folders: folder,
                    exclude: if include_all { vec![] } else { outlook::default_exclude() },
                    window: window.window(),
                    delete: delete.rule(),
                    full_rescan,
                }),
                DateWindow::default(),
                None,
            ),
            ImportCmd::Imap { host, port, user, password_env, folder, include_all, src, plaintext, window, delete } => (
                Source::Imap(imap::ImapConfig {
                    host,
                    port,
                    user,
                    src,
                    folders: folder,
                    exclude: if include_all { vec![] } else { imap::DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect() },
                    plaintext,
                    window: window.window(),
                    delete: delete.rule(),
                    ..Default::default()
                }),
                DateWindow::default(),
                Some(password_env),
            ),
            ImportCmd::Pop3 { host, port, user, password_env, src, plaintext, window, delete } => (
                Source::Pop3(pop3::Pop3Config { host, port, user, password: String::new(), src, plaintext, window: window.window(), delete: delete.rule() }),
                DateWindow::default(),
                Some(password_env),
            ),
        };
        Profile { name: name.to_string(), source, window, password_env, last_run: None }
    }
}

fn abs(p: PathBuf) -> PathBuf {
    std::fs::canonicalize(&p).map(|c| {
        // Avoid Windows "\\?\" verbatim prefixes in saved profiles.
        let s = c.to_string_lossy().to_string();
        PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
    }).unwrap_or(p)
}

#[derive(Subcommand, Clone)]
enum ProfileCmd {
    /// Save an archiving profile, e.g.
    /// `nea profile add "Work Outlook" outlook --older-than 90 --delete-older-than 365`
    Add {
        name: String,
        #[command(subcommand)]
        source: ImportCmd,
    },
    /// List saved profiles.
    List,
    /// Run one profile, or all of them with --all.
    Run {
        name: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Delete a saved profile (archived mail is kept).
    Remove { name: String },
    /// Put a double-click launcher for a profile on the desktop.
    Shortcut { name: String },
    /// Run a profile every day at a set time (Windows Task Scheduler;
    /// prints a crontab line elsewhere).
    Schedule {
        name: String,
        /// Time of day, HH:MM (24-hour).
        #[arg(long, default_value = "12:30")]
        daily: String,
        /// Remove the schedule instead.
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum SortArg {
    Relevance,
    Newest,
    Oldest,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Format {
    Eml,
    Mbox,
    /// Outlook .msg files
    Msg,
}

fn opts(cli: &Cli, writer: bool) -> OpenOptions {
    OpenOptions {
        cache_dir: cli.cache_dir.clone(),
        cache_in_archive: cli.cache_in_archive,
        writer,
        break_lock: cli.break_lock,
        create: false,
    }
}

fn progress_line(s: &ImportStats, t0: Instant) {
    let n = s.added + s.duplicates + s.new_locations;
    let rate = n as f64 / t0.elapsed().as_secs_f64().max(0.001);
    eprint!(
        "\r  {} new, {} already archived, {} new locations, {} failed{}{}  ({:.0} msg/s)   ",
        s.added,
        s.duplicates,
        s.new_locations,
        s.failed,
        if s.skipped > 0 { format!(", {} outside date range", s.skipped) } else { String::new() },
        if s.deleted > 0 { format!(", {} deleted from source", s.deleted) } else { String::new() },
        rate
    );
    let _ = std::io::stderr().flush();
}

fn report(s: &ImportStats, t0: Instant) {
    progress_line(s, t0);
    eprintln!("\nDone in {:.1}s.", t0.elapsed().as_secs_f64());
    for e in s.errors.iter().take(20) {
        eprintln!("  ! {e}");
    }
    if s.errors.len() > 20 {
        eprintln!("  … {} more", s.errors.len() - 20);
    }
}

fn fmt_date(ts: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(ts, 0).single().map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cmd = cli.cmd.clone().unwrap_or(Cmd::Serve { port: 8765, open: true });
    match &cmd {
        Cmd::Init => {
            let mut o = opts(&cli, false);
            o.create = true;
            let a = Archive::open(&cli.archive, &o)?;
            println!("Archive ready at {} (id {})", a.root.display(), a.manifest.id);
            println!("Search cache: {}", a.cache.display());
        }
        Cmd::Import(ic) => {
            let mut o = opts(&cli, true);
            o.create = true;
            let mut a = Archive::open(&cli.archive, &o)?;
            let p = ic.to_profile("import:cli");
            run_profile(&mut a, &p)?;
        }
        Cmd::Profile(pc) => {
            let mut o = opts(&cli, !matches!(pc, ProfileCmd::List));
            o.create = matches!(pc, ProfileCmd::Add { .. });
            let mut a = Archive::open(&cli.archive, &o)?;
            match pc {
                ProfileCmd::Add { name, source } => {
                    let p = source.to_profile(name);
                    p.source_window().validate()?;
                    println!("Saved profile {name:?}: {}", p.describe());
                    nea::profile::upsert(&a, p)?;
                }
                ProfileCmd::List => {
                    let all = nea::profile::load(&a.root)?;
                    if all.is_empty() {
                        println!("No profiles yet. Add one with `nea profile add <name> <source> ...`.");
                    }
                    for p in all {
                        let last = p.last_run.map(fmt_date).unwrap_or_else(|| "never".into());
                        println!("{:24}  last run {:10}  {}", p.name, last, p.describe());
                    }
                }
                ProfileCmd::Run { name, all } => {
                    let list = match (name, all) {
                        (Some(n), false) => vec![nea::profile::find(&a.root, n)?],
                        (None, true) => nea::profile::load(&a.root)?,
                        _ => bail!("give a profile name or --all"),
                    };
                    let mut failed = 0;
                    for p in &list {
                        eprintln!("== {} : {}", p.name, p.describe());
                        if let Err(e) = run_profile(&mut a, p) {
                            eprintln!("  ! {e:#}");
                            failed += 1;
                        }
                    }
                    if failed > 0 {
                        bail!("{failed} of {} profiles failed", list.len());
                    }
                }
                ProfileCmd::Shortcut { name } => {
                    let p = nea::profile::find(&a.root, name)?;
                    let path = make_shortcut(&a.root, &p.name)?;
                    println!("Created {}", path.display());
                    if p.needs_password() && p.password_env.as_deref().and_then(|v| std::env::var(v).ok()).is_none() {
                        println!("It will ask for the password each time it runs.");
                    }
                }
                ProfileCmd::Schedule { name, daily, remove } => {
                    let p = nea::profile::find(&a.root, name)?;
                    schedule(&a.root, &p, daily, *remove)?;
                }
                ProfileCmd::Remove { name } => {
                    if nea::profile::remove(&a, name)? {
                        println!("Removed profile {name:?}.");
                    } else {
                        bail!("no profile named {name:?}");
                    }
                }
            }
        }
        Cmd::Detect => {
            let found = nea::detect::detect();
            if found.is_empty() {
                println!("No mail programs' data found in the usual places.");
            }
            for f in found {
                if f.kind == "outlook" {
                    println!("{:8} {}   ->  nea import outlook", f.kind, f.label);
                } else {
                    println!("{:8} {}   ->  nea import {} \"{}\"", f.kind, f.label, f.kind, f.path.display());
                }
            }
        }
        Cmd::OutlookFolders => {
            for (store, path, count) in outlook::list_folders()? {
                println!("{count:>8}  {store} / {path}");
            }
        }
        Cmd::Search { query, sort, limit, json } => {
            let a = Archive::open(&cli.archive, &opts(&cli, false))?;
            let q = query.join(" ");
            let sort = sort.map(|s| match s {
                SortArg::Relevance => Sort::Relevance,
                SortArg::Newest => Sort::Newest,
                SortArg::Oldest => Sort::Oldest,
            });
            let r = a.index.search(&q, sort, 0, *limit)?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!("{} results in {} ms   [{}]", r.total, r.took_ms, r.explain);
                for h in &r.hits {
                    let from: String = h.from.chars().take(28).collect();
                    let subj: String = h.subject.chars().take(60).collect();
                    println!("{}  {:10}  {:28}  {}", &h.id[..12], fmt_date(h.date), from, subj);
                }
            }
        }
        Cmd::Show { id } => {
            let a = Archive::open(&cli.archive, &opts(&cli, false))?;
            let id = resolve_id(&a, id)?;
            let raw = a.read_raw(&id)?;
            let p = nea::parse::parse(&raw);
            println!("Subject: {}\nFrom: {}\nDate: {}\nFolders: {:?}\n", p.subject, p.from_display(), fmt_date(p.date), a.catalog.locations(&id)?);
            println!("{}", p.body_text);
            for at in &p.attachments {
                println!("[attachment] {} ({}, {} bytes)", at.name, at.content_type, at.size);
            }
        }
        Cmd::Export { query, format, out } => {
            let a = Archive::open(&cli.archive, &opts(&cli, false))?;
            let ids = a.index.all_ids(&query.join(" "))?;
            let t0 = Instant::now();
            let st = match format {
                Format::Eml => nea::export::to_eml_dir(&a, &ids, out)?,
                Format::Mbox => nea::export::to_mbox(&a, &ids, out)?,
                Format::Msg => nea::export::to_msg_dir(&a, &ids, out)?,
            };
            println!("Exported {} messages ({} failed) in {:.1}s to {}", st.written, st.failed, t0.elapsed().as_secs_f64(), out.display());
        }
        Cmd::ExportImap { query, host, port, user, password_env, folder, prefix, plaintext } => {
            let a = Archive::open(&cli.archive, &opts(&cli, false))?;
            let ids = a.index.all_ids(&query.join(" "))?;
            let password = password_from(password_env, &format!("{user}@{host}"))?;
            let t = nea::deliver::ImapTarget { host: host.clone(), port: *port, user: user.clone(), password, folder: folder.clone(), prefix: prefix.clone(), plaintext: *plaintext };
            eprintln!("Copying {} messages to {host}…", ids.len());
            let s = nea::deliver::to_imap(&a, &ids, &t, |s| eprint!("\r  {} copied, {} already there, {} failed   ", s.sent, s.skipped, s.failed))?;
            eprintln!();
            for e in &s.errors {
                eprintln!("  ! {e}");
            }
        }
        Cmd::ExportEmail { query, to, from, host, port, user, password_env, plaintext } => {
            let a = Archive::open(&cli.archive, &opts(&cli, false))?;
            let ids = a.index.all_ids(&query.join(" "))?;
            let password = match user {
                Some(u) => password_from(password_env, &format!("{u}@{host}"))?,
                None => String::new(),
            };
            let security = if *plaintext {
                nea::deliver::Security::Plain
            } else if *port == 465 {
                nea::deliver::Security::Tls
            } else {
                nea::deliver::Security::Starttls
            };
            let t = nea::deliver::SmtpTarget { host: host.clone(), port: *port, security, user: user.clone(), password, from: from.clone(), to: to.clone() };
            eprintln!("Sending {} messages to {}…", ids.len(), to.join(", "));
            let s = nea::deliver::to_smtp(&a, &ids, &t, |s| eprint!("\r  {} sent, {} failed   ", s.sent, s.failed))?;
            eprintln!();
            for e in &s.errors {
                eprintln!("  ! {e}");
            }
        }
        Cmd::Delete { query, yes } => {
            let q = query.join(" ");
            if q.trim().is_empty() {
                bail!("give a search that selects the messages to delete");
            }
            let mut a = Archive::open(&cli.archive, &opts(&cli, true))?;
            let ids = a.index.all_ids(&q)?;
            if ids.is_empty() {
                println!("No messages match.");
                return Ok(());
            }
            if !*yes {
                eprint!("Delete {} messages matching {q:?} from the archive? Type yes: ", ids.len());
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if answer.trim() != "yes" {
                    println!("Nothing deleted.");
                    return Ok(());
                }
            }
            let n = a.delete_messages(&ids)?;
            println!("Deleted {n} messages. Run `nea compact` to reclaim their space.");
        }
        Cmd::Compact => {
            let mut a = Archive::open(&cli.archive, &opts(&cli, true))?;
            let r = a.compact(|d, t| eprint!("\r  {d}/{t} messages moved"))?;
            eprintln!();
            println!(
                "{} messages moved, {} packs removed, {:.1} MB -> {:.1} MB",
                r.moved,
                r.packs_removed,
                r.bytes_before as f64 / 1e6,
                r.bytes_after as f64 / 1e6
            );
            for w in &r.warnings {
                eprintln!("  ! {w}");
            }
        }
        Cmd::Backup { dest } => {
            let t0 = Instant::now();
            let r = nea::backup::backup(&cli.archive, dest, |r| eprint!("\r  {} files copied, {} updated, {:.1} MB   ", r.copied, r.appended, r.bytes as f64 / 1e6))?;
            eprintln!();
            println!(
                "Backup up to date in {:.1}s: {} copied, {} updated, {} unchanged, {} removed, {:.1} MB written",
                t0.elapsed().as_secs_f64(),
                r.copied,
                r.appended,
                r.unchanged,
                r.removed,
                r.bytes as f64 / 1e6
            );
        }
        Cmd::Verify => {
            let a = Archive::open(&cli.archive, &opts(&cli, false))?;
            let r = a.verify(|n| eprint!("\r  checked {n}"))?;
            eprintln!();
            println!("{} files checked, {} ok, {} corrupt, {} missing, {} unknown files", r.checked, r.ok, r.corrupt.len(), r.missing.len(), r.unknown_files);
            for c in r.corrupt.iter().take(20) {
                println!("  corrupt: {c}");
            }
            for m in r.missing.iter().take(20) {
                println!("  missing: {m}");
            }
            if !r.corrupt.is_empty() || !r.missing.is_empty() {
                std::process::exit(2);
            }
        }
        Cmd::Rebuild => {
            let mut a = Archive::open(&cli.archive, &opts(&cli, false))?;
            let t0 = Instant::now();
            a.rebuild_index(|n, t| eprint!("\r  indexed {n}/{t}"))?;
            eprintln!("\nRebuilt in {:.1}s", t0.elapsed().as_secs_f64());
        }
        Cmd::Stats => {
            let a = Archive::open(&cli.archive, &opts(&cli, false))?;
            let s = a.catalog.stats()?;
            println!("Archive: {}\nCache:   {}", a.root.display(), a.cache.display());
            println!("{} messages, {:.1} MB, {} attachments, {} indexed", s.messages, s.bytes as f64 / 1e6, s.attachments, a.index.doc_count());
            if let (Some(f), Some(l)) = (s.first_date, s.last_date) {
                println!("From {} to {}", fmt_date(f), fmt_date(l));
            }
            for f in a.catalog.folders()? {
                println!("  {:>7}  {} / {}", f.count, f.src, f.folder);
            }
        }
        Cmd::Serve { port, open } => {
            let mut o = opts(&cli, false);
            o.create = true;
            let a = Archive::open(&cli.archive, &o).with_context(|| format!("opening archive {}", cli.archive.display()))?;
            o.create = false;
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            rt.block_on(nea::server::serve(a, o, *port, *open))?;
        }
        Cmd::Bench { messages } => bench(*messages)?,
        Cmd::Demo { messages, out } => {
            write_synth_mbox(out, *messages)?;
            println!("Wrote {messages} demo messages to {}", out.display());
        }
    }
    Ok(())
}

/// The command line that runs a profile against this archive.
fn run_command(root: &std::path::Path, name: &str) -> Result<(PathBuf, Vec<String>)> {
    if name.contains(['"', '%', '\n', '\r', '`', '$']) {
        bail!("rename the profile without quotes, %, $ or backticks to make a launcher for it");
    }
    let exe = std::env::current_exe()?;
    Ok((exe, vec!["-a".into(), root.display().to_string(), "profile".into(), "run".into(), name.to_string()]))
}

fn desktop_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_default();
    let d = home.join("Desktop");
    if d.is_dir() {
        d
    } else {
        home
    }
}

fn make_shortcut(root: &std::path::Path, name: &str) -> Result<PathBuf> {
    let (exe, args) = run_command(root, name)?;
    let quoted: Vec<String> = std::iter::once(exe.display().to_string()).chain(args).map(|a| format!("\"{a}\"")).collect();
    let file = nea::export::sanitize(&format!("Archive mail - {name}"), 80);
    let path;
    if cfg!(windows) {
        path = desktop_dir().join(format!("{file}.cmd"));
        std::fs::write(&path, format!("@echo off\r\ntitle Natendo Email Archiver: {name}\r\n{}\r\npause\r\n", quoted.join(" ")))?;
    } else {
        let ext = if cfg!(target_os = "macos") { "command" } else { "sh" };
        path = desktop_dir().join(format!("{file}.{ext}"));
        std::fs::write(&path, format!("#!/bin/sh\n{}\nprintf 'Press Enter to close'; read _\n", quoted.join(" ")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(path)
}

fn schedule(root: &std::path::Path, p: &Profile, daily: &str, remove: bool) -> Result<()> {
    let t = chrono::NaiveTime::parse_from_str(daily, "%H:%M").context("--daily takes a time like 12:30")?;
    let (exe, args) = run_command(root, &p.name)?;
    let task = format!("Natendo Email Archiver\\{}", p.name);
    if cfg!(windows) {
        let status = if remove {
            std::process::Command::new("schtasks").args(["/Delete", "/TN", &task, "/F"]).status()?
        } else {
            if p.needs_password() && p.password_env.as_deref().and_then(|v| std::env::var(v).ok()).is_none() {
                bail!("a scheduled run can't ask for a password; set the environment variable named in the profile (password_env) for your user first");
            }
            let tr = std::iter::once(exe.display().to_string()).chain(args).map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(" ");
            std::process::Command::new("schtasks")
                .args(["/Create", "/TN", &task, "/TR", &tr, "/SC", "DAILY", "/ST", &t.format("%H:%M").to_string(), "/F"])
                .status()?
        };
        if !status.success() {
            bail!("Task Scheduler refused the change");
        }
        println!("{} \"{}\"", if remove { "Removed schedule for" } else { "Scheduled daily at" }, if remove { p.name.clone() } else { format!("{} : {}", t.format("%H:%M"), p.name) });
    } else {
        let line = std::iter::once(exe.display().to_string()).chain(args).map(|a| format!("'{}'", a.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ");
        println!("Add this line with `crontab -e` to run it daily at {}:", t.format("%H:%M"));
        println!("{} {} * * * {line}", t.format("%M"), t.format("%H"));
    }
    Ok(())
}

fn password_from(env: &str, label: &str) -> Result<String> {
    match std::env::var(env) {
        Ok(p) => Ok(p),
        Err(_) => Ok(rpassword::prompt_password(format!("Password for {label}: "))?),
    }
}

fn run_profile(a: &mut Archive, p: &Profile) -> Result<()> {
    let t0 = Instant::now();
    let label = match &p.source {
        Source::Imap(c) => format!("{}@{}", c.user, c.host),
        Source::Pop3(c) => format!("{}@{}", c.user, c.host),
        _ => String::new(),
    };
    let stats = nea::profile::run(
        a,
        p,
        || Ok(rpassword::prompt_password(format!("Password for {label}: "))?),
        |f, s| {
            eprint!("\r[{f}] ");
            progress_line(s, t0)
        },
    )?;
    a.commit()?;
    report(&stats, t0);
    Ok(())
}

fn resolve_id(a: &Archive, prefix: &str) -> Result<String> {
    if prefix.len() == 64 {
        return Ok(prefix.to_string());
    }
    let mut stmt = a.catalog.conn.prepare("SELECT id FROM messages WHERE id LIKE ?1 || '%' LIMIT 2")?;
    let ids: Vec<String> = stmt.query_map([prefix], |r| r.get(0))?.collect::<Result<_, _>>()?;
    match ids.len() {
        1 => Ok(ids[0].clone()),
        0 => bail!("no message with id {prefix}"),
        _ => bail!("id prefix {prefix} is ambiguous"),
    }
}

fn write_synth_mbox(path: &std::path::Path, n: u64) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut w = std::io::BufWriter::new(std::fs::File::create(path)?);
    for i in 0..n {
        writeln!(w, "From demo@local Mon Jan  1 00:00:00 2024")?;
        w.write_all(&nea::synth::message(7, i))?;
        writeln!(w)?;
    }
    w.flush()?;
    Ok(())
}

fn bench(n: u64) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("nea-bench-{}", std::process::id()));
    let src_dir = dir.join("src");
    std::fs::create_dir_all(&src_dir)?;
    println!("Generating {n} synthetic messages…");
    let mbox_path = src_dir.join("bench.mbox");
    write_synth_mbox(&mbox_path, n)?;
    let o = OpenOptions { cache_dir: Some(dir.join("cache")), writer: true, create: true, ..Default::default() };
    let mut a = Archive::open(&dir.join("archive"), &o)?;
    let t0 = Instant::now();
    let st = mbox::import(&mut a, &mbox_path, "bench", |_| {})?;
    let secs = t0.elapsed().as_secs_f64();
    println!("Import: {} messages in {:.2}s = {:.0} msg/s", st.added, secs, st.added as f64 / secs);
    let queries = [
        "invoice", "lease renewal", "\"flight itinerary\"", "from:dana", "heatr repiar", "has:attachment receipt",
        "after:2020 before:2022 budget", "invoice OR receipt -newsletter", "domain:acme.com", "#4471", "landlord fix heating",
        "subject:contract", "type:txt", "", "year:2019",
    ];
    let mut times = Vec::new();
    for round in 0..5 {
        for q in &queries {
            let t = Instant::now();
            let r = a.index.search(q, None, 0, 50)?;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            times.push(ms);
            if round == 0 {
                println!("  {:>6} hits  {:6.1} ms  {}", r.total, ms, q);
            }
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| times[((times.len() as f64 - 1.0) * q) as usize];
    println!("Search latency (incl. facets + snippets): p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms", p(0.5), p(0.95), p(1.0));
    drop(a);
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
