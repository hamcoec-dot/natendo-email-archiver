# MailStore Home feature parity

Every feature described in the MailStore Home manual (sections: Installation, Archiving Email, Accessing the Archive, Exporting Email, Backup and Restore, Administration, Protecting Archive Access), mapped to Natendo Email Archiver. Status is updated as work lands.

Legend: ✅ done · 🟡 partial · ⬜ not yet · ➖ intentionally different

## Archiving sources

| MailStore Home | Natendo | Status |
|---|---|---|
| IMAP accounts (auto-detected settings) | `import imap`; settings auto-detected for common providers planned | 🟡 |
| POP3 accounts ("Leave message on server") | POP3 source | ⬜ |
| Microsoft Exchange server | via IMAP today; EWS/Graph needs app registration (decision) | 🟡 |
| Gmail, Outlook.com, AOL (via IMAP) | Gmail with labels; others via IMAP | ✅ |
| Microsoft Outlook profiles on this PC (pick profile/identity, pick folders) | Outlook bridge (COM automation of classic Outlook) | ⬜ |
| Windows Mail / Windows Live Mail stores | auto-detect their .eml store folders | ⬜ |
| Mozilla Thunderbird / SeaMonkey profiles | MBOX import of the profile folder; auto-detect planned | 🟡 |
| Email files (EML, MSG, PST, MBOX) | all four (PST experimental) | ✅ |
| Archive only a date range / messages older than N days | per-run and per-profile filters | ⬜ |
| Delete messages from the source after archiving (flagged mail never deleted) | deletion rules for Outlook and IMAP | ⬜ |
| Saved archiving profiles, run again later | profiles | ⬜ |
| Desktop shortcut that runs a profile | `nea profile run` + shortcut / scheduled task | ⬜ |
| Incremental runs (only new mail) | IMAP UIDs; Outlook by received date | 🟡 |

## Accessing the archive

| MailStore Home | Natendo | Status |
|---|---|---|
| Folder tree ("My Archive") | folder facet and folder filter; full tree view planned | 🟡 |
| Quick search, wildcards `*` and `?`, phrases, `-` exclusion | yes, plus typo tolerance and stemming | ✅ |
| Field search: subject, from, to, cc, bcc, body (`bcontent:`), attachments (`acontent:`) | `subject: from: to: cc: bcc: body: att:`; MailStore's `bcontent:`/`acontent:` accepted as aliases | 🟡 |
| Search Email form: folder (+ subfolders), from, to/cc/bcc, date range | operators + facets; form planned | 🟡 |
| Archiving date range | `archived:` | ⬜ |
| With / without attachments | `has:attachment`, `-has:attachment` | ✅ |
| Size range | `larger:` `smaller:` | ✅ |
| Priority (low / high) | `priority:` | ⬜ |
| Saved search folders (rename, delete, view query) | saved searches | 🟡 |
| Open, reply, forward in mail client; print | download .eml; open in mail client and print planned | 🟡 |

## Exporting

| MailStore Home | Natendo | Status |
|---|---|---|
| Export to an IMAP / Exchange / Gmail mailbox (restore) | `export imap` (APPEND) | ⬜ |
| Send archived messages to an address via SMTP | SMTP export | ⬜ |
| Export into installed mail clients | via EML/MBOX files; Outlook via the bridge | ⬜ |
| Directory: EML files | ✅ | ✅ |
| Directory: Outlook MSG files | MSG writer | ⬜ |
| Export from a saved search | export any query | ✅ |
| Export profiles (saved, re-runnable) | profiles | ⬜ |

## Backup, administration, protection

| MailStore Home | Natendo | Status |
|---|---|---|
| Portable install (USB) | single binary + `--cache-in-archive` | ✅ |
| Backup to HDD/USB | `nea backup` (incremental, packs are append-only) | ⬜ |
| Restore / choose archive location | open any archive folder | ✅ |
| Attachment indexing by file type (IFilters) | built-in extractors, no IFilters needed | ➖ |
| Rebuild index | `nea rebuild` | ✅ |
| Free unused disk space (after deleting from the archive) | delete from archive + compaction | ⬜ |
| Check data integrity | `nea verify` | ✅ |
| Update check (opt-in) | not planned yet (no telemetry) | ➖ |
| Password protection with recovery key | archive encryption with recovery key | ⬜ |
