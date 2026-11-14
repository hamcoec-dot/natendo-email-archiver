# MailStore Home feature parity

Every feature described in the MailStore Home manual (sections: Installation, Archiving Email, Accessing the Archive, Exporting Email, Backup and Restore, Administration, Protecting Archive Access), mapped to Natendo Email Archiver. Status is updated as work lands.

Legend: ✅ done · 🟡 partial · ⬜ not yet · ➖ intentionally different

## Archiving sources

| MailStore Home | Natendo | Status |
|---|---|---|
| IMAP accounts | `nea import imap`, or Archive mail → IMAP in the app | ✅ |
| POP3 accounts ("Leave message on server") | `nea import pop3`; mail is left on the server unless a delete rule is set | ✅ |
| Microsoft Exchange server | via IMAP (or via Outlook on this PC); a native EWS/Graph source needs an Azure app registration | 🟡 |
| Gmail, Outlook.com, AOL (via IMAP) | Gmail archived once from All Mail with labels kept as folders; others via IMAP | ✅ |
| Microsoft Outlook profiles on this PC (pick mailboxes and folders) | `nea import outlook`, Archive mail → Outlook on this PC with a folder picker; drives classic Outlook through its automation interface | ✅ (tested against a simulated Outlook; a real-Outlook run is still to do on a PC with classic Outlook set up) |
| Windows Mail / Windows Live Mail stores | found by `nea detect` / "Find mail on this computer", imported as EML folders | ✅ |
| Mozilla Thunderbird / SeaMonkey profiles | found automatically; whole profiles import with clean folder names | ✅ |
| Email files (EML, MSG, PST, MBOX) | all four (PST experimental) | ✅ |
| Archive only a date range / messages older than N days | `--after --before --older-than` on every source, and in profiles | ✅ |
| Delete messages from the source after archiving | `--delete-older-than DAYS` for Outlook, IMAP and POP3. Flagged mail is never deleted, unread mail is kept unless `--include-unread`, and only mail the archive provably holds is deleted. Outlook deletes go to Deleted Items | ✅ |
| Saved archiving profiles, run again later | `nea profile add/list/run/remove`, and Saved profiles in the app | ✅ |
| Desktop shortcut that runs a profile | `nea profile shortcut NAME`; also `nea profile schedule NAME --daily 12:30` (Windows Task Scheduler, crontab line elsewhere) | ✅ |
| Incremental runs (only new mail) | IMAP by UID, POP3 by Message-ID and size, Outlook by received date, files by content hash | ✅ |

## Accessing the archive

| MailStore Home | Natendo | Status |
|---|---|---|
| Folder tree ("My Archive") | folder facet and `infolder:` filter; a full tree view is planned | 🟡 |
| Quick search, wildcards `*` and `?`, phrases, `-` exclusion | yes, plus typo tolerance and stemming | ✅ |
| Field search: subject, from, to, cc, bcc, body, attachments | `subject: from: to: cc: bcc: body: att:`; MailStore's `bcontent:`/`acontent:` work too | ✅ |
| Search Email form: folder (+ subfolders), from, to/cc/bcc, date range | search operators, facets and a syntax help panel; a form is planned | 🟡 |
| Archiving date range | `archived:2024`, `archivedafter:`, `archivedbefore:` | ✅ |
| With / without attachments | `has:attachment`, `-has:attachment` | ✅ |
| Size range | `larger:` `smaller:` | ✅ |
| Priority (low / high) | `priority:high` / `low` / `normal` | ✅ |
| Saved search folders | saved searches (star button) | ✅ |
| Open, reply, forward in mail client; print | "Open in mail app" (default mail program), "Print", "Download .eml" | ✅ |

## Exporting

| MailStore Home | Natendo | Status |
|---|---|---|
| Export to an IMAP / Exchange / Gmail mailbox (restore) | `nea export-imap`, Export → Copy into an email account. Folders recreated, dates and read state kept, re-runs skip copies already there | ✅ |
| Send archived messages to an address via SMTP | `nea export-email`, Export → Send by email | ✅ |
| Export into installed mail clients | via EML / MSG / MBOX files, or IMAP into the account the client uses | ✅ |
| Directory: EML files | `--format eml` | ✅ |
| Directory: Outlook MSG files | `--format msg` (checked with an independent MSG reader) | ✅ |
| Export from a saved search | export any search | ✅ |
| Export profiles (saved, re-runnable) | re-run from the command line or a saved search; saved export profiles are not built | 🟡 |

## Backup, administration, protection

| MailStore Home | Natendo | Status |
|---|---|---|
| Archive on a network share or NAS | supported and designed for it (MailStore Home does not allow this) | ✅ |
| Portable install (USB) | single binary + `--cache-in-archive` | ✅ |
| Backup to HDD/USB | `nea backup DEST`, Archive → Back up now. Incremental, and the backup opens as an archive | ✅ |
| Restore / choose archive location | open any archive folder or backup | ✅ |
| Attachment indexing by file type (IFilters) | built-in extractors, no IFilters needed | ➖ |
| Rebuild index | `nea rebuild` (also automatic when the index format changes) | ✅ |
| Delete from the archive, free unused disk space | `nea delete`, Delete buttons; `nea compact` / "Reclaim space" | ✅ |
| Check data integrity | `nea verify` | ✅ |
| Update check (opt-in) | not planned (no network calls of our own) | ➖ |
| Password protection with recovery key | use your own encryption, e.g. a Cryptomator vault, BitLocker or VeraCrypt (see README, "Encrypting your archive") | ➖ |
