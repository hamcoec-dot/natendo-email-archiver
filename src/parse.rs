//! MIME parsing: turns raw RFC 5322 bytes into the fields the archive indexes.

use mail_parser::{Address, HeaderValue, MessageParser, MimeHeaders};

use crate::extract;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Addr {
    pub name: String,
    pub email: String,
}

impl Addr {
    pub fn display(&self) -> String {
        match (self.name.is_empty(), self.email.is_empty()) {
            (false, false) => format!("{} <{}>", self.name, self.email),
            (true, false) => self.email.clone(),
            _ => self.name.clone(),
        }
    }
    pub fn domain(&self) -> String {
        self.email.rsplit_once('@').map(|(_, d)| d.to_lowercase()).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Attachment {
    pub index: usize,
    pub name: String,
    pub content_type: String,
    pub size: usize,
    /// Extracted text (None when the format is not supported).
    #[serde(skip)]
    pub text: Option<String>,
    pub content_id: Option<String>,
    pub inline: bool,
}

impl Attachment {
    pub fn extension(&self) -> String {
        extract::extension_of(&self.name, &self.content_type)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ParsedMessage {
    pub message_id: String,
    pub subject: String,
    pub from: Vec<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub bcc: Vec<Addr>,
    /// Unix seconds. Falls back to 0 when the message has no usable Date.
    pub date: i64,
    pub body_text: String,
    pub body_html: Option<String>,
    pub attachments: Vec<Attachment>,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    /// Gmail labels (from Google Takeout's X-Gmail-Labels header).
    pub gmail_labels: Vec<String>,
    /// 1 = high, 3 = normal, 5 = low (X-Priority / Importance / Priority).
    pub priority: u8,
}

impl ParsedMessage {
    /// A stable key grouping a conversation: the root of the References chain.
    pub fn thread_key(&self) -> String {
        if let Some(r) = self.references.first() {
            return r.clone();
        }
        if let Some(r) = self.in_reply_to.first() {
            return r.clone();
        }
        if !self.message_id.is_empty() {
            return self.message_id.clone();
        }
        String::new()
    }

    pub fn from_display(&self) -> String {
        self.from.iter().map(Addr::display).collect::<Vec<_>>().join(", ")
    }
}

fn addrs(a: Option<&Address>) -> Vec<Addr> {
    let mut out = Vec::new();
    let Some(a) = a else { return out };
    let push = |out: &mut Vec<Addr>, x: &mail_parser::Addr| {
        out.push(Addr {
            name: x.name.as_deref().unwrap_or("").trim().to_string(),
            email: x.address.as_deref().unwrap_or("").trim().to_lowercase(),
        })
    };
    match a {
        Address::List(list) => list.iter().for_each(|x| push(&mut out, x)),
        Address::Group(groups) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .for_each(|x| push(&mut out, x)),
    }
    out
}

fn text_list(v: &HeaderValue) -> Vec<String> {
    match v {
        HeaderValue::Text(t) => vec![t.to_string()],
        HeaderValue::TextList(l) => l.iter().map(|t| t.to_string()).collect(),
        _ => vec![],
    }
}

/// Normalize the different priority headers to 1 (high), 3 (normal) or 5 (low).
pub fn priority(x_priority: Option<&str>, importance: Option<&str>) -> u8 {
    if let Some(x) = x_priority {
        let x = x.trim().to_ascii_lowercase();
        if let Some(d) = x.chars().next().and_then(|c| c.to_digit(10)) {
            return match d {
                1 | 2 => 1,
                4 | 5 => 5,
                _ => 3,
            };
        }
        if x.starts_with("high") {
            return 1;
        }
        if x.starts_with("low") {
            return 5;
        }
    }
    match importance.map(|s| s.trim().to_ascii_lowercase()) {
        Some(s) if s.starts_with("high") || s.starts_with("urgent") => 1,
        Some(s) if s.starts_with("low") || s.starts_with("non-urgent") => 5,
        _ => 3,
    }
}

/// Parse a raw message. Never fails: unparseable input yields an empty message
/// whose body is the lossy text of the input, so nothing is ever unsearchable.
pub fn parse(raw: &[u8]) -> ParsedMessage {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return ParsedMessage {
            body_text: String::from_utf8_lossy(raw).into_owned(),
            ..Default::default()
        };
    };
    let mut p = ParsedMessage {
        message_id: msg.message_id().unwrap_or("").to_string(),
        subject: msg.subject().unwrap_or("").to_string(),
        from: addrs(msg.from()),
        to: addrs(msg.to()),
        cc: addrs(msg.cc()),
        bcc: addrs(msg.bcc()),
        date: msg.date().map(|d| d.to_timestamp()).unwrap_or(0),
        in_reply_to: text_list(msg.in_reply_to()),
        references: text_list(msg.references()),
        ..Default::default()
    };
    p.priority = priority(
        msg.header_raw("X-Priority").or_else(|| msg.header_raw("X-MSMail-Priority")),
        msg.header_raw("Importance").or_else(|| msg.header_raw("Priority")),
    );
    if let Some(labels) = msg.header_raw("X-Gmail-Labels") {
        p.gmail_labels = labels
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    // Body: concatenate every text body part (multipart/alternative shows one).
    let mut body = String::new();
    for i in 0..msg.text_body.len() {
        if let Some(t) = msg.body_text(i) {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&t);
        }
    }
    p.body_text = body;
    p.body_html = msg.body_html(0).map(|h| h.into_owned());

    for (i, part) in msg.attachments().enumerate() {
        let ct = part
            .content_type()
            .map(|c| match c.subtype() {
                Some(s) => format!("{}/{}", c.ctype(), s),
                None => c.ctype().to_string(),
            })
            .unwrap_or_else(|| "application/octet-stream".into())
            .to_lowercase();
        let inline = part
            .content_disposition()
            .map(|d| d.ctype().eq_ignore_ascii_case("inline"))
            .unwrap_or(false);
        let mut name = part.attachment_name().unwrap_or("").to_string();
        // Nested messages: index their text too.
        let (text, size) = if let Some(nested) = part.message() {
            if name.is_empty() {
                name = format!("{}.eml", nested.subject().unwrap_or("message"));
            }
            let raw_nested = nested.raw_message();
            let n = parse(raw_nested);
            let t = format!(
                "{}\n{}\n{}",
                n.subject,
                n.from_display(),
                n.body_text
            );
            (Some(t), raw_nested.len())
        } else {
            let data = part.contents();
            (extract::extract_text(&name, &ct, data), data.len())
        };
        if name.is_empty() {
            name = format!("attachment-{}", i + 1);
        }
        p.attachments.push(Attachment {
            index: i,
            name,
            content_type: ct,
            size,
            text,
            content_id: part.content_id().map(|s| s.to_string()),
            inline,
        });
    }
    p
}

/// Return the raw bytes and metadata of the attachment with this index.
pub fn attachment_bytes(raw: &[u8], index: usize) -> Option<(String, String, Vec<u8>)> {
    let msg = MessageParser::default().parse(raw)?;
    let part = msg.attachments().nth(index)?;
    let ct = part
        .content_type()
        .map(|c| match c.subtype() {
            Some(s) => format!("{}/{}", c.ctype(), s),
            None => c.ctype().to_string(),
        })
        .unwrap_or_else(|| "application/octet-stream".into());
    let name = part.attachment_name().unwrap_or("attachment").to_string();
    let data = match part.message() {
        Some(m) => m.raw_message().to_vec(),
        None => part.contents().to_vec(),
    };
    Some((name, ct, data))
}
