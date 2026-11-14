# Status and open decisions (2026-11-14)

## What works

| Area | State | Verified by |
|---|---|---|
| Archive format: append-only packs + JSONL journal + writer lock | Done | unit + end-to-end tests, crash/torn-tail tests |
| Archive on USB / NAS, several computers reading | Done (local cache per computer, journal replay) | `second_machine_catches_up_from_journal`, bulk multi-cache test |
| Import: folder of .eml | Done | end-to-end tests |
| Import: MailStore Home export (EML and MSG) | Done | end-to-end + MSG conversion tests (synthetic MSG) |
| Import: MBOX, Google Takeout labels, Thunderbird folders | Done | end-to-end tests |
| Import: IMAP, incremental, download-once dedupe | Done | in-process mock IMAP server |
| Import: Gmail (All Mail once, labels as folders) | Done | mock server with X-GM-EXT-1 |
| Import: PST/OST | Experimental | opens an empty PST only; needs a real file |
| Search: Gmail-style operators, stemming, typos, phrases, symbols, any-word fallback | Done | `search_features` test |
| Attachment text: PDF, Office, OpenDocument, RTF, HTML, text, ZIP | Done (no OCR) | tests for DOCX/TXT; PDF via pdf-extract |
| Facets with drill-down | Done | tests |
| Export EML/MBOX (CLI + UI), verify, rebuild | Done | round-trip test is byte-for-byte |
| Web UI | Done | driven in a browser: search, typos, HTML sandbox, tracker blocked, import, export, saved searches |
| Desktop window (`nea-desktop`) | Done (Windows checked) | launched; window + WebView2 up; API answered |
| CI for Windows/macOS/Linux | Written, not yet run (repo not published) | |

Benchmarks (Windows 11 desktop, synthetic mail): 250,000 messages import at 7,783 msg/s; search median 8.2 ms, p95 49 ms including facets and snippets.

## Open decisions

1. **Publish the repo.** Creating public `hamcoec-dot/natendo-email-archiver` was blocked by auto mode overnight. Approve it in the thread and I will create it, push, and let CI run on all three OSes.
2. **Storage format.** Messages live in append-only pack files (`messages/pack-NNNNNN.nmp`, documented in the README) instead of one `.eml` per message. This made imports 13x faster and is much kinder to USB drives and NAS shares. Raw messages are still byte-for-byte originals and `nea export` writes plain `.eml`/`.mbox` at any time. Keep packs (recommended) or switch to loose files?
3. **Gmail/Microsoft sign-in.** Today Gmail and Outlook.com work over IMAP with an app password. OAuth needs registering the app with Google (verification for restricted Gmail scopes) and Microsoft. Worth starting that process?
4. **OCR and semantic search.** Both need extra components: OCR via Windows' built-in engine (Windows only) or bundled Tesseract (~30 MB); semantic search via a local embedding model (~100 MB download). Which first, and is a model download acceptable?
5. **Installers and signing.** MSI/DMG/AppImage packaging and code signing (a Windows code-signing certificate costs money). Wanted for v0.2?
6. **Credentials.** IMAP passwords are prompted or read from an environment variable and never stored. Scheduled background archiving would need them saved in the OS keychain. OK to add?

## Known limits

- PST import is unverified on real data; try it on a copy of a real .pst.
- Deleting messages from the archive (retention) is not built; packs would need compaction.
- English stemming only; other languages are searched unstemmed (typo tolerance still applies).
- The UI's import box takes a typed path (no file picker yet in the browser UI).
