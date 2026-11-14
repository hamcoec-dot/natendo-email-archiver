//! Attachment text extraction. Pure Rust, no dependency on OS filters
//! (MailStore relies on Windows iFilters, which are often missing).
//!
//! Supported: PDF, DOCX/XLSX/PPTX, ODT/ODS/ODP, HTML, RTF, plain-text family,
//! ZIP (names + text of inner files). OCR for images/scanned PDFs is a
//! planned background stage (see README).

use std::io::Read;

/// Upper bound of extracted text kept per attachment.
pub const MAX_TEXT: usize = 1_000_000;
/// Attachments larger than this are not text-extracted.
const MAX_INPUT: usize = 64 * 1024 * 1024;

pub fn extension_of(name: &str, content_type: &str) -> String {
    if let Some((_, ext)) = name.rsplit_once('.') {
        let ext = ext.trim().to_lowercase();
        if !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return ext;
        }
    }
    match content_type {
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/calendar" => "ics",
        "message/rfc822" => "eml",
        ct if ct.starts_with("image/") => &ct[6..],
        _ => "",
    }
    .to_string()
}

fn cap(mut s: String) -> String {
    if s.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

pub fn extract_text(name: &str, content_type: &str, data: &[u8]) -> Option<String> {
    if data.is_empty() || data.len() > MAX_INPUT {
        return None;
    }
    let ext = extension_of(name, content_type);
    let text = match ext.as_str() {
        "pdf" => pdf(data),
        "docx" | "docm" | "dotx" => office_xml(data, |n| n == "word/document.xml" || n.starts_with("word/header") || n.starts_with("word/footer")),
        "xlsx" | "xlsm" => office_xml(data, |n| n == "xl/sharedStrings.xml" || n.starts_with("xl/worksheets/sheet")),
        "pptx" => office_xml(data, |n| n.starts_with("ppt/slides/slide") || n.starts_with("ppt/notesSlides/")),
        "odt" | "ods" | "odp" => office_xml(data, |n| n == "content.xml"),
        "html" | "htm" | "xhtml" => Some(html_to_text(&String::from_utf8_lossy(data))),
        "rtf" => Some(rtf_to_text(&String::from_utf8_lossy(data))),
        "txt" | "csv" | "tsv" | "md" | "log" | "json" | "xml" | "ics" | "vcf" | "ini" | "cfg"
        | "yaml" | "yml" | "sql" | "py" | "js" | "ts" | "rs" | "c" | "h" | "cpp" | "cs"
        | "java" | "sh" | "ps1" | "bat" => Some(String::from_utf8_lossy(data).into_owned()),
        "zip" => zip_listing(data),
        _ if content_type.starts_with("text/") => Some(String::from_utf8_lossy(data).into_owned()),
        _ => None,
    }?;
    let t = text.trim();
    if t.is_empty() {
        None
    } else {
        Some(cap(t.to_string()))
    }
}

thread_local! {
    static QUIET_PANICS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install (once) a panic hook that stays silent for panics inside
/// third-party extractors on this thread, and defers to the default otherwise.
fn install_quiet_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !QUIET_PANICS.with(|q| q.get()) {
                prev(info);
            }
        }));
    });
}

fn pdf(data: &[u8]) -> Option<String> {
    // pdf-extract can panic on malformed files; never let one attachment
    // take down an import.
    install_quiet_hook();
    QUIET_PANICS.with(|q| q.set(true));
    let r = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(data).ok());
    QUIET_PANICS.with(|q| q.set(false));
    r.ok().flatten()
}

fn office_xml(data: &[u8], want: impl Fn(&str) -> bool) -> Option<String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).ok()?;
    let mut names: Vec<String> = zip.file_names().filter(|n| want(n)).map(String::from).collect();
    names.sort_by_key(|n| natural_key(n));
    let mut out = String::new();
    for n in names {
        let mut f = zip.by_name(&n).ok()?;
        let mut s = String::new();
        if f.read_to_string(&mut s).is_ok() {
            out.push_str(&xml_to_text(&s));
            out.push('\n');
        }
        if out.len() > MAX_TEXT {
            break;
        }
    }
    Some(out)
}

/// Sort "slide10.xml" after "slide9.xml".
fn natural_key(s: &str) -> (String, u64) {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    let prefix: String = s.chars().filter(|c| !c.is_ascii_digit()).collect();
    (prefix, digits.parse().unwrap_or(0))
}

fn zip_listing(data: &[u8]) -> Option<String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).ok()?;
    let mut out = String::new();
    for i in 0..zip.len().min(2000) {
        let Ok(mut f) = zip.by_index(i) else { continue };
        let name = f.name().to_string();
        out.push_str(&name);
        out.push('\n');
        if f.size() as usize <= 8 * 1024 * 1024 && !f.is_dir() {
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_ok() {
                let ext = extension_of(&name, "");
                if ext != "zip" {
                    if let Some(t) = extract_text(&name, "", &buf) {
                        out.push_str(&t);
                        out.push('\n');
                    }
                }
            }
        }
        if out.len() > MAX_TEXT {
            break;
        }
    }
    Some(out)
}

pub fn xml_to_text(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() / 3);
    let mut in_tag = false;
    let mut tag = String::new();
    for c in xml.chars() {
        if in_tag {
            if c == '>' {
                in_tag = false;
                let t = tag.trim_start_matches('/');
                // Paragraph/row/cell ends become separators.
                if tag.starts_with('/')
                    && (t == "w:p" || t == "a:p" || t == "text:p" || t == "row" || t == "text:h" || t == "si")
                {
                    out.push('\n');
                } else if t == "w:tab" || t == "c" || t == "table:table-cell" || t.starts_with("w:br") {
                    out.push(' ');
                }
                tag.clear();
            } else {
                tag.push(c);
            }
        } else if c == '<' {
            in_tag = true;
        } else {
            out.push(c);
        }
    }
    unescape_entities(&out)
}

pub fn unescape_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if let Some(j) = rest[..rest.len().min(12)].find(';') {
            let ent = &rest[1..j];
            let rep = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                e if e.starts_with("#x") || e.starts_with("#X") => {
                    u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32)
                }
                e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            if let Some(c) = rep {
                out.push(c);
                rest = &rest[j + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

pub fn html_to_text(html: &str) -> String {
    // Drop script/style blocks, then tags.
    let lower = html.to_lowercase();
    let mut cleaned = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        let rest = &lower[i..];
        let skip = ["<script", "<style", "<head"].iter().find(|t| rest.starts_with(**t)).map(|t| {
            let close = format!("</{}", &t[1..]);
            rest.find(&close).map(|e| e + close.len()).unwrap_or(rest.len())
        });
        if let Some(n) = skip {
            i += n;
            continue;
        }
        let c = html[i..].chars().next().unwrap();
        cleaned.push(c);
        i += c.len_utf8();
    }
    let mut out = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    for c in cleaned.chars() {
        if in_tag {
            if c == '>' {
                in_tag = false;
                let t = tag.trim_start_matches('/').to_lowercase();
                let name: String = t.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
                if matches!(name.as_str(), "br" | "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "table") {
                    out.push('\n');
                } else {
                    out.push(' ');
                }
                tag.clear();
            } else {
                tag.push(c);
            }
        } else if c == '<' {
            in_tag = true;
        } else {
            out.push(c);
        }
    }
    let text = unescape_entities(&out);
    // Collapse runs of blank space.
    let mut res = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        let l = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if l.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        res.push_str(&l);
        res.push('\n');
    }
    res
}

/// Crude RTF to text: drops control words and groups like \fonttbl.
pub fn rtf_to_text(rtf: &str) -> String {
    let mut out = String::new();
    let mut depth_skip: Option<usize> = None;
    let mut depth = 0usize;
    let chars: Vec<char> = rtf.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '{' => {
                depth += 1;
                i += 1;
                if depth_skip.is_none() {
                    let rest: String = chars[i..chars.len().min(i + 12)].iter().collect();
                    if ["\\fonttbl", "\\colortbl", "\\stylesheet", "\\info", "\\*", "\\pict"]
                        .iter()
                        .any(|k| rest.starts_with(k))
                    {
                        depth_skip = Some(depth);
                    }
                }
            }
            '}' => {
                if depth_skip == Some(depth) {
                    depth_skip = None;
                }
                depth = depth.saturating_sub(1);
                i += 1;
            }
            '\\' => {
                i += 1;
                let mut word = String::new();
                while i < chars.len() && chars[i].is_ascii_alphabetic() {
                    word.push(chars[i]);
                    i += 1;
                }
                if word.is_empty() && i < chars.len() {
                    let sym = chars[i];
                    i += 1;
                    if sym == '\'' && i + 1 < chars.len() {
                        let hex: String = chars[i..i + 2].iter().collect();
                        if let Ok(b) = u8::from_str_radix(&hex, 16) {
                            if depth_skip.is_none() {
                                out.push(b as char);
                            }
                        }
                        i += 2;
                    } else if depth_skip.is_none() && matches!(sym, '\\' | '{' | '}') {
                        out.push(sym);
                    }
                    continue;
                }
                // numeric parameter
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '-') {
                    i += 1;
                }
                if i < chars.len() && chars[i] == ' ' {
                    i += 1;
                }
                if depth_skip.is_none() && (word == "par" || word == "line") {
                    out.push('\n');
                } else if depth_skip.is_none() && word == "tab" {
                    out.push('\t');
                }
            }
            '\r' | '\n' => i += 1,
            _ => {
                if depth_skip.is_none() {
                    out.push(c);
                }
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_strips_tags_and_scripts() {
        let t = html_to_text("<html><head><title>x</title></head><body><script>bad()</script><p>Hello&nbsp;<b>World</b></p><p>Two &amp; three</p></body></html>");
        assert!(t.contains("Hello World"));
        assert!(t.contains("Two & three"));
        assert!(!t.contains("bad()"));
    }

    #[test]
    fn rtf_basic() {
        let t = rtf_to_text(r"{\rtf1\ansi{\fonttbl\f0 Arial;}\f0 Hello \b World\b0\par Next}");
        assert!(t.contains("Hello World"));
        assert!(t.contains("Next"));
        assert!(!t.contains("Arial"));
    }

    #[test]
    fn docx_text() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            z.start_file::<_, ()>("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(b"<w:document><w:body><w:p><w:r><w:t>Lease renewal</w:t></w:r></w:p><w:p><w:r><w:t>Unit 4B</w:t></w:r></w:p></w:body></w:document>").unwrap();
            z.finish().unwrap();
        }
        let t = extract_text("lease.docx", "", buf.get_ref()).unwrap();
        assert!(t.contains("Lease renewal"));
        assert!(t.contains("Unit 4B"));
    }

    #[test]
    fn extension_fallback() {
        assert_eq!(extension_of("a.PDF", ""), "pdf");
        assert_eq!(extension_of("noext", "application/pdf"), "pdf");
    }
}
