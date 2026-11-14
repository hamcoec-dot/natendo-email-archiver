<p align="center"><img src="assets/icons/natendo-app-icon-small.svg" width="72" alt=""></p>

# Natendo Email Archiver

Archive all your email in one local archive you own, then **find anything in it in milliseconds**.

Natendo Email Archiver pulls mail from IMAP accounts (Gmail, Outlook.com, Fastmail, any server), MBOX files, Google Takeout, Thunderbird, folders of `.eml` files, and MailStore Home exports into a single deduplicated archive. Its focus is search: typo-tolerant, stemmed, ranked full-text search over bodies **and attachments**, Gmail-style operators, live facets, and a query language that never throws a syntax error.

> Status: early MVP (v0.1). Desktop window, local web UI and command line. Windows, macOS and Linux. See [Roadmap](#roadmap).

## Highlights

- **Search that finds things**: BM25 ranking with a recency prior, English stemming (`receipts` finds `receipt`), typo tolerance (`invoce` finds `invoice`), phrases with wildcards (`"order 44*"`), prefix names (`from:dan`), exclusions anywhere (`invoice OR -newsletter`), symbols preserved (`#4471`, `C++`, `user_id=99`).
- **Attachment text** from PDF, Word, Excel, PowerPoint, OpenDocument, RTF, HTML, text and ZIP contents, extracted in pure Rust (no Windows iFilters needed).
- **Facets** for sender domain and address, year and month, folder and attachment type, with live counts. Click to narrow, Shift+click to exclude.
- **Archive anywhere**: local disk, USB drive, or a NAS/network share. Several computers can read the same archive; one writes at a time.
- **Fast**: about 7,800 messages/second import and 8 ms median search (with facets and snippets) on a 250,000-message archive. See [Performance](#performance).
- **Open, durable format**: messages are stored byte-for-byte as received, content-addressed by SHA-256, in documented append-only files. Everything else is rebuildable.
- **Private**: runs entirely on your computer. The UI listens on `127.0.0.1` only; HTML mail is sandboxed and remote images are blocked until you allow them.

## Quick start

Build (Rust 1.80+):

```bash
cargo build --release
```

The binary is `target/release/nea` (`nea.exe` on Windows).

For the desktop app (the same UI in a native window, using the system web view):

```bash
cargo build --release --features desktop
```

This adds `nea-desktop` (`nea-desktop.exe`), which opens your archive (default `Documents/Natendo Archive`, or a path given as its argument) in its own window. On Linux it needs `libwebkit2gtk-4.1-dev` and `libgtk-3-dev` to build.

Try it with demo mail:

```bash
nea demo --messages 5000 --out demo.mbox
nea --archive MyArchive import mbox demo.mbox
nea --archive MyArchive serve --open
```

Archive real mail:

```bash
# Gmail: use an app password (Google Account > Security > App passwords)
nea --archive MyArchive import imap --host imap.gmail.com --user you@gmail.com

# Any IMAP server; password from an environment variable instead of a prompt
NEA_IMAP_PASSWORD=... nea --archive MyArchive import imap --host imap.fastmail.com --user you@fastmail.com

# Files
nea --archive MyArchive import mbox "Takeout/Mail/All mail Including Spam and Trash.mbox"
nea --archive MyArchive import mbox "C:/Users/me/AppData/Roaming/Thunderbird/Profiles/xyz/Mail"
nea --archive MyArchive import eml "D:/Old mail"          # .eml and Outlook .msg files
nea --archive MyArchive import pst "D:/Outlook/archive.pst"   # experimental
```

Running an IMAP import again only fetches new mail (per-folder UIDVALIDITY and last UID are tracked). On Gmail it archives `All Mail` once and records each message's labels as folders, so labelled mail downloads once.

Search from the command line:

```bash
nea --archive MyArchive search from:dana has:attachment after:2019
nea --archive MyArchive search '"lease renewal" type:pdf' --json
```

Set `NEA_ARCHIVE=MyArchive` to skip `--archive`. Without `--archive` the archive is `Documents/Natendo Archive`, and running `nea` with no command opens the web UI, so double-clicking `nea.exe` just works.

## Moving from MailStore Home

MailStore Home keeps mail in an undocumented private store, so Natendo imports MailStore's own standard export instead:

1. In MailStore Home choose **Export Email**, then **Directory (File System)**, format **EML**, and export your archive folders.
2. Run `nea --archive MyArchive import mailstore "D:/MailStore Export"` (or use **Import** in the web UI).

Account and folder structure is rebuilt from the export's folder tree (a top-level folder that looks like an email address becomes the source account). Messages already archived from another source are deduplicated by content hash. MSG exports work too (they are converted to standard MIME, keeping the original Internet headers when Outlook stored them), but EML is preferred because it is byte-for-byte the original.

## Search syntax

| You type | Means |
|---|---|
| `invoice acme` | All words; stemmed, typo-tolerant, ranked |
| `"purchase order 44*"` | Phrase; trailing `*` matches word starts |
| `invoice OR receipt -newsletter` | Boolean; `-` excludes, works inside OR groups; `( )` groups |
| `from:dana to:bob cc:priya` | People; partial names and addresses work |
| `sender:dana@acme.com` `domain:acme.com` | Exact sender or sender domain |
| `subject:lease` `body:heater` | Only that field |
| `att:"lease renewal"` | Only inside attachment text |
| `has:attachment` `type:pdf` `filename:*.xlsx` | Attachments |
| `after:2019-03 before:2020` | Date range (year, month or day; `MM/DD/YYYY` too) |
| `date:"last month"` `year:2021` `month:2021-07` | Calendar periods: today, yesterday, this/last week/month/year, last summer, march 2021, last 3 months |
| `newer_than:30d` `older_than:2y` | Relative age |
| `larger:5M` `smaller:100k` | Size |
| `folder:receipts` `in:inbox` `label:travel` | Folders and Gmail labels |
| `infolder:"you@gmail.com/INBOX"` `source:mailstore` | Exact folder path or source |
| `#4471` `C++` `user_id=99` | Symbols kept intact |
| `~heater ~landlord` | Any of these words (broad recall) |

Anything not understood is searched as plain text, so a query is never an error.

## Web UI

`nea serve --open` starts the UI at `http://127.0.0.1:8765/`: search-as-you-type, facets, highlighted snippets (including inside attachments), threaded conversations, sender autocomplete for `from:`/`to:`, a sandboxed message viewer, attachment download, `.eml` download, and an import dialog for files and MailStore exports. Keyboard: `/` search, `j`/`k` next/previous, `Esc` back.

The UI is a single static page over a small JSON API (`/api/search`, `/api/message/:id`, ...), which the desktop shell (`nea-desktop`, built on the same webview libraries Tauri uses) loads unchanged.

## How it works

```text
<archive>/                      anywhere: local disk, USB, NAS share
  archive.json                  identity and format version
  messages/pack-000001.nmp      raw messages, append-only packs
  journal/000001.jsonl          append-only catalog: locations, sync state, imports
  writer.lock                   present while a writer is active

<local cache>/                  per computer, rebuildable at any time
  catalog.sqlite                metadata, contacts, folder counts
  index/                        Tantivy full-text index
```

- **The archive folder is the source of truth** and holds no live database, so it is safe on network shares where SQLite and index locking are unreliable. Only append-only files are written there, by a single writer holding `writer.lock` (created atomically; stale locks from crashed processes are reclaimed).
- **Each computer keeps its own cache** (SQLite + Tantivy) on local disk, by default in the OS cache folder (`%LOCALAPPDATA%\NatendoEmailArchiver`, `~/Library/Caches/...`, `~/.cache/...`). On open, and every 20 seconds while the UI runs, it replays new journal lines, so a second computer sees mail the first one archived. `--cache-in-archive` keeps the cache inside the archive folder for a fully portable USB setup; `--cache-dir` puts it anywhere.
- **Pack format** (so mail is recoverable without this app): each record is a line `NEAMSG1 <sha256> <length>`, then exactly `length` bytes of the original message, then a newline. Packs roll over at 128 MB and are never rewritten. Message bytes are fsynced before the journal references them, so a crash cannot leave the catalog pointing at missing mail.
- **Dedupe**: messages are identified by the SHA-256 of their bytes. The same message seen in several folders or accounts is stored once with several locations. For non-Gmail IMAP servers, messages already archived from another folder (same Message-ID and size) get a new location without being downloaded again.
- **Search**: Tantivy (a Lucene-class engine in Rust) with fields for subject, body, from, to/cc, attachment names and text, folders, a raw-token field for symbols, and facet fields. Queries are parsed by a forgiving Gmail-style parser into boosted boolean queries (subject 3x, sender 2x, attachments 0.7x; fuzzy variants at 0.3x so exact matches rank first).

### Maintenance commands

```bash
nea verify     # re-hash every message; reports corrupt or missing ones (exit code 2 on problems)
nea rebuild    # rebuild the search index from the archive
nea stats      # counts, date range, folders
nea export 'from:dana after:2020' --format mbox --out dana.mbox
nea export --format eml --out "D:/Export"     # everything, as .eml files in folders
```

## Performance

`nea bench` on a Windows 11 desktop (release build, synthetic mail):

| Archive size | Import (parse, dedupe, store, index) | Search median | Search 95th percentile |
|---|---|---|---|
| 50,000 messages | 6,543 messages/s | 5.7 ms | 17.4 ms |
| 250,000 messages | 7,783 messages/s | 8.2 ms | 49.2 ms |

Search times include facet counts and highlighted snippets. At this rate a 1,000,000-message archive imports from local files in about two minutes (network speed limits IMAP).

Run the benchmark on your own machine; CI runs it on Windows, macOS and Linux for every commit.

## Development

```bash
cargo test            # unit + end-to-end tests
cargo run -- --help
```

Source map: `src/archive.rs` (storage and sync), `src/pack.rs`, `src/journal.rs`, `src/lock.rs`, `src/index.rs` (Tantivy schema, search, facets), `src/query.rs` (query language), `src/parse.rs` + `src/extract.rs` (MIME and attachment text), `src/import/` (EML/MailStore, MBOX, IMAP), `src/export.rs`, `src/server.rs` + `ui/index.html` (web UI).

## Roadmap

- Installers (MSI/DMG/AppImage), tray icon and scheduled background archiving for the desktop app.
- Gmail and Microsoft 365 sign-in with OAuth (today: IMAP with app passwords).
- PST/OST import is experimental (verified on an empty PST only; needs real-file testing); restore to IMAP.
- OCR for scanned PDFs and images; optional local semantic search ("the email where the landlord agreed to fix the heater").
- Saved searches, contact directory, attachment gallery.
- Optional archive encryption.

## License

Apache License 2.0. See [LICENSE](LICENSE). Current status and open decisions: [docs/STATUS.md](docs/STATUS.md). Not affiliated with MailStore or any email provider.
