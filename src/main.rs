use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use nea::archive::{Archive, ImportStats, OpenOptions};
use nea::import::{eml, imap, mbox};
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

#[derive(Subcommand, Clone)]
enum ImportCmd {
    /// A folder of .eml files (subfolders become folders).
    Eml {
        dir: PathBuf,
        #[arg(long, default_value = "files")]
        src: String,
    },
    /// A MailStore Home "Export to directory (EML)" folder.
    Mailstore {
        dir: PathBuf,
        #[arg(long, default_value = "mailstore")]
        src: String,
    },
    /// An mbox file, Google Takeout mbox, or Thunderbird profile folder.
    Mbox {
        path: PathBuf,
        #[arg(long, default_value = "mbox")]
        src: String,
    },
    /// An Outlook .pst or .ost file (experimental).
    Pst {
        path: PathBuf,
        #[arg(long, default_value = "outlook")]
        src: String,
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
        "\r  {} new, {} already archived, {} new locations, {} failed  ({:.0} msg/s)   ",
        s.added, s.duplicates, s.new_locations, s.failed, rate
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
            let t0 = Instant::now();
            let prog = |s: &ImportStats| progress_line(s, t0);
            let stats = match ic {
                ImportCmd::Eml { dir, src } => eml::import(&mut a, dir, src, eml::Flavor::Files, prog)?,
                ImportCmd::Mailstore { dir, src } => eml::import(&mut a, dir, src, eml::Flavor::MailStore, prog)?,
                ImportCmd::Mbox { path, src } => mbox::import(&mut a, path, src, prog)?,
                ImportCmd::Pst { path, src } => nea::import::pst::import(&mut a, path, src, prog)?,
                ImportCmd::Imap { host, port, user, password_env, folder, include_all, src, plaintext } => {
                    let password = match std::env::var(password_env) {
                        Ok(p) => p,
                        Err(_) => rpassword::prompt_password(format!("Password for {user}@{host}: "))?,
                    };
                    let cfg = imap::ImapConfig {
                        host: host.clone(),
                        port: *port,
                        user: user.clone(),
                        password,
                        src: src.clone(),
                        folders: folder.clone(),
                        exclude: if *include_all { vec![] } else { imap::DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect() },
                        plaintext: *plaintext,
                    };
                    imap::import(&mut a, &cfg, |f, s| {
                        eprint!("[{f}] ");
                        progress_line(s, t0)
                    })?
                }
            };
            a.commit()?;
            report(&stats, t0);
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
            };
            println!("Exported {} messages ({} failed) in {:.1}s to {}", st.written, st.failed, t0.elapsed().as_secs_f64(), out.display());
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
