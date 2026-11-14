//! Forgiving, Gmail-style query language. Nothing is ever a syntax error:
//! unknown fields become plain words, unbalanced quotes/parens are closed.
//!
//! ```text
//! invoice acme                 all words, stemmed, typo-tolerant, ranked
//! "purchase order 44*"         phrase, trailing wildcard allowed
//! from:dana to:bob cc:x        address fields (prefix match: from:dan)
//! subject:lease body:x att:x   field-restricted text (att: = attachment text)
//! has:attachment type:pdf filename:*.xlsx
//! after:2019-03 before:2020 date:"last month" year:2021 month:2021-07
//! newer_than:30d older_than:1y larger:5M smaller:100k
//! folder:receipts in:inbox label:travel source:gmail
//! domain:acme.com sender:dana@acme.com
//! invoice OR receipt -newsletter (a OR b) -(c d)
//! #4471 C++ user_id=99         special characters preserved (raw tokens)
//! ~heater landlord             "any of these words" recall mode
//! ```

use std::ops::Bound;

use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, PhrasePrefixQuery, PhraseQuery, Query, RangeQuery,
    RegexQuery, TermQuery,
};
use tantivy::schema::{Facet, Field, IndexRecordOption};
use tantivy::{Index, Term};

use crate::index::{clean_raw, Fields};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    Relevance,
    Newest,
    Oldest,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Word(String),
    Phrase(String),
    Field { name: String, value: String },
    Or(Vec<Node>),
    And(Vec<Node>),
    Not(Box<Node>),
    /// `~words`: match any word (recall mode).
    Any(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Parsed {
    pub items: Vec<Node>,
}

const FIELDS: &[&str] = &[
    "from", "to", "cc", "bcc", "subject", "body", "att", "attachment", "filename", "has", "type", "ext", "after",
    "before", "date", "on", "year", "month", "newer_than", "older_than", "newer", "older", "larger", "smaller",
    "size", "folder", "in", "label", "infolder", "domain", "sender", "source", "src", "id", "msgid",
    "rfc822msgid", "thread",
];

// ---------------------------------------------------------------- lexing

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Open,
    Close,
    Neg,
    Or,
    Word(String),
    Phrase(String),
    Field(String, String),
    Tilde(String),
}

fn lex(s: &str) -> Vec<Tok> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    let read_quoted = |i: &mut usize| -> String {
        // assumes chars[*i] is an opening quote
        let close = if chars[*i] == '“' { '”' } else { '"' };
        *i += 1;
        let mut v = String::new();
        while *i < chars.len() && chars[*i] != close && chars[*i] != '"' {
            v.push(chars[*i]);
            *i += 1;
        }
        *i += 1; // closing quote (or past end)
        v
    };
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => {
                out.push(Tok::Open);
                i += 1;
            }
            ')' => {
                out.push(Tok::Close);
                i += 1;
            }
            '|' => {
                out.push(Tok::Or);
                i += 1;
            }
            '-' if i + 1 < chars.len() && !chars[i + 1].is_whitespace() => {
                out.push(Tok::Neg);
                i += 1;
            }
            '"' | '“' => out.push(Tok::Phrase(read_quoted(&mut i))),
            _ => {
                let start = i;
                let mut w = String::new();
                while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '(' && chars[i] != ')' {
                    // field:"quoted value"
                    if chars[i] == ':' && i + 1 < chars.len() && (chars[i + 1] == '"' || chars[i + 1] == '“') {
                        let name = w.to_lowercase();
                        if FIELDS.contains(&name.as_str()) {
                            i += 1;
                            let v = read_quoted(&mut i);
                            out.push(Tok::Field(name, v));
                            w.clear();
                            break;
                        }
                    }
                    w.push(chars[i]);
                    i += 1;
                }
                if w.is_empty() {
                    if i == start {
                        i += 1;
                    }
                    continue;
                }
                if w == "OR" {
                    out.push(Tok::Or);
                } else if w == "AND" {
                    // implicit
                } else if let Some(rest) = w.strip_prefix('~') {
                    if !rest.is_empty() {
                        out.push(Tok::Tilde(rest.to_string()));
                    }
                } else if let Some((name, value)) = w.split_once(':') {
                    let lname = name.to_lowercase();
                    if FIELDS.contains(&lname.as_str()) && !value.is_empty() {
                        out.push(Tok::Field(lname, value.to_string()));
                    } else {
                        out.push(Tok::Word(w));
                    }
                } else {
                    out.push(Tok::Word(w));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- parsing

pub fn parse(s: &str) -> Parsed {
    let toks = lex(s);
    let mut pos = 0;
    let mut items = parse_seq(&toks, &mut pos, false);
    // Merge consecutive ~words into one recall group.
    let mut merged: Vec<Node> = Vec::new();
    for n in items.drain(..) {
        if let (Node::Any(new), Some(Node::Any(prev))) = (&n, merged.last_mut()) {
            prev.extend(new.iter().cloned());
            continue;
        }
        merged.push(n);
    }
    Parsed { items: merged }
}

fn parse_seq(toks: &[Tok], pos: &mut usize, nested: bool) -> Vec<Node> {
    let mut items: Vec<Node> = Vec::new();
    let mut pending_or = false;
    while *pos < toks.len() {
        match &toks[*pos] {
            Tok::Close => {
                *pos += 1;
                if nested {
                    break;
                }
                continue; // stray ')'
            }
            Tok::Or => {
                *pos += 1;
                if !items.is_empty() {
                    pending_or = true;
                }
                continue;
            }
            _ => {}
        }
        let Some(node) = parse_unary(toks, pos) else { continue };
        if pending_or {
            pending_or = false;
            let prev = items.pop().unwrap();
            let group = match prev {
                Node::Or(mut v) => {
                    v.push(node);
                    Node::Or(v)
                }
                other => Node::Or(vec![other, node]),
            };
            items.push(group);
        } else {
            items.push(node);
        }
    }
    items
}

fn parse_unary(toks: &[Tok], pos: &mut usize) -> Option<Node> {
    let t = toks.get(*pos)?.clone();
    *pos += 1;
    Some(match t {
        Tok::Neg => Node::Not(Box::new(parse_unary(toks, pos)?)),
        Tok::Open => {
            let inner = parse_seq(toks, pos, true);
            match inner.len() {
                0 => return None,
                1 => inner.into_iter().next().unwrap(),
                _ => Node::And(inner),
            }
        }
        Tok::Word(w) => Node::Word(w),
        Tok::Phrase(p) => {
            if p.trim().is_empty() {
                return None;
            }
            Node::Phrase(p)
        }
        Tok::Field(n, v) => Node::Field { name: n, value: v },
        Tok::Tilde(w) => Node::Any(vec![w]),
        Tok::Close | Tok::Or => return None,
    })
}

impl Parsed {
    /// True when the query has positive free-text terms (relevance sort makes sense).
    pub fn has_text_terms(&self) -> bool {
        fn text(n: &Node) -> bool {
            match n {
                Node::Word(_) | Node::Phrase(_) | Node::Any(_) => true,
                Node::Field { name, .. } => matches!(name.as_str(), "subject" | "body" | "att" | "attachment"),
                Node::Or(v) | Node::And(v) => v.iter().any(text),
                Node::Not(_) => false,
            }
        }
        self.items.iter().any(text)
    }

    /// The same query with its plain words turned into "any of" (filters
    /// and phrases stay required). None when there is nothing to relax.
    pub fn relaxed(&self) -> Option<Parsed> {
        let words: Vec<String> = self
            .items
            .iter()
            .filter_map(|n| match n {
                Node::Word(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        if words.len() < 2 {
            return None;
        }
        let mut items: Vec<Node> = self.items.iter().filter(|n| !matches!(n, Node::Word(_))).cloned().collect();
        items.push(Node::Any(words));
        Some(Parsed { items })
    }

    pub fn explain(&self) -> String {
        fn ex(n: &Node) -> String {
            match n {
                Node::Word(w) => w.clone(),
                Node::Phrase(p) => format!("\"{p}\""),
                Node::Field { name, value } => match name.as_str() {
                    "after" | "before" | "date" | "on" | "year" | "month" | "newer_than" | "older_than" | "newer"
                    | "older" => match date_range(name, value) {
                        Some((lo, hi)) => format!("{} {}", name, describe_range(lo, hi)),
                        None => format!("{name}:{value} (date not understood)"),
                    },
                    _ => format!("{name}:{value}"),
                },
                Node::Or(v) => format!("({})", v.iter().map(ex).collect::<Vec<_>>().join(" OR ")),
                Node::And(v) => format!("({})", v.iter().map(ex).collect::<Vec<_>>().join(" ")),
                Node::Not(n) => format!("NOT {}", ex(n)),
                Node::Any(v) => format!("any of [{}]", v.join(", ")),
            }
        }
        self.items.iter().map(ex).collect::<Vec<_>>().join(" · ")
    }
}

// ---------------------------------------------------------------- dates

fn local_ts(d: NaiveDate) -> i64 {
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .map(|t| t.timestamp())
        .unwrap_or_else(|| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
}

fn add_months(d: NaiveDate, m: i32) -> NaiveDate {
    let total = d.year() * 12 + d.month0() as i32 + m;
    NaiveDate::from_ymd_opt(total.div_euclid(12), total.rem_euclid(12) as u32 + 1, 1).unwrap_or(d)
}

/// A calendar span [start, end) for a date expression.
pub fn date_span(v: &str, today: NaiveDate) -> Option<(NaiveDate, NaiveDate)> {
    let v = v.trim().to_lowercase().replace('_', " ");
    let ymd = |y: i32, m: u32, d: u32| NaiveDate::from_ymd_opt(y, m, d);
    // Numeric forms
    let parts: Vec<&str> = v.split(['-', '/', '.']).collect();
    if parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
        let n: Vec<i64> = parts.iter().filter_map(|p| p.parse().ok()).collect();
        match (n.len(), parts[0].len()) {
            (1, 4) => {
                let s = ymd(n[0] as i32, 1, 1)?;
                return Some((s, ymd(n[0] as i32 + 1, 1, 1)?));
            }
            (2, 4) => {
                let s = ymd(n[0] as i32, n[1] as u32, 1)?;
                return Some((s, add_months(s, 1)));
            }
            (3, 4) => {
                let s = ymd(n[0] as i32, n[1] as u32, n[2] as u32)?;
                return Some((s, s + Duration::days(1)));
            }
            (3, _) if parts[2].len() == 4 => {
                // US style MM/DD/YYYY
                let s = ymd(n[2] as i32, n[0] as u32, n[1] as u32)?;
                return Some((s, s + Duration::days(1)));
            }
            _ => {}
        }
    }
    let month_start = ymd(today.year(), today.month(), 1)?;
    let week_start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    let season = |name: &str, year: i32| -> Option<(NaiveDate, NaiveDate)> {
        let (sm, len) = match name {
            "spring" => (3, 3),
            "summer" => (6, 3),
            "fall" | "autumn" => (9, 3),
            "winter" => (12, 3),
            _ => return None,
        };
        let s = ymd(year, sm, 1)?;
        Some((s, add_months(s, len)))
    };
    let words: Vec<&str> = v.split_whitespace().collect();
    match words.as_slice() {
        ["today"] => Some((today, today + Duration::days(1))),
        ["yesterday"] => Some((today - Duration::days(1), today)),
        ["this", "week"] => Some((week_start, week_start + Duration::days(7))),
        ["last", "week"] => Some((week_start - Duration::days(7), week_start)),
        ["this", "month"] => Some((month_start, add_months(month_start, 1))),
        ["last", "month"] => Some((add_months(month_start, -1), month_start)),
        ["this", "year"] => Some((ymd(today.year(), 1, 1)?, ymd(today.year() + 1, 1, 1)?)),
        ["last", "year"] => Some((ymd(today.year() - 1, 1, 1)?, ymd(today.year(), 1, 1)?)),
        ["last", n, unit] | ["past", n, unit] => {
            let n: i64 = n.parse().ok()?;
            let start = match unit.trim_end_matches('s') {
                "day" => today - Duration::days(n),
                "week" => today - Duration::days(7 * n),
                "month" => add_months(today, -(n as i32)),
                "year" => add_months(today, -(n as i32) * 12),
                _ => return None,
            };
            Some((start, today + Duration::days(1)))
        }
        ["last", s] if season(s, 2000).is_some() => {
            // Most recent completed season.
            let mut y = today.year();
            loop {
                let (a, b) = season(s, y)?;
                if b <= today {
                    return Some((a, b));
                }
                y -= 1;
                let _ = a;
            }
        }
        ["this", s] if season(s, 2000).is_some() => season(s, today.year()),
        [s, y] if season(s, 2000).is_some() => season(s, y.parse().ok()?),
        [m] | [m, _] if month_num(m).is_some() => {
            let mo = month_num(m)?;
            let year = match words.get(1) {
                Some(y) => y.parse().ok()?,
                None => {
                    if mo <= today.month() {
                        today.year()
                    } else {
                        today.year() - 1
                    }
                }
            };
            let s = ymd(year, mo, 1)?;
            Some((s, add_months(s, 1)))
        }
        _ => None,
    }
}

fn month_num(s: &str) -> Option<u32> {
    const M: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    if s.len() < 3 {
        return None;
    }
    M.iter().position(|m| s.starts_with(m)).map(|i| i as u32 + 1)
}

/// Relative age like 30d, 2w, 6m, 1y.
fn age_start(v: &str, today: NaiveDate) -> Option<NaiveDate> {
    let v = v.trim().to_lowercase();
    let (num, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    Some(match unit {
        "d" | "day" | "days" => today - Duration::days(n),
        "w" | "week" | "weeks" => today - Duration::days(7 * n),
        "m" | "month" | "months" => add_months(today, -(n as i32)),
        "y" | "year" | "years" => add_months(today, -(n as i32) * 12),
        _ => return None,
    })
}

/// Timestamp bounds for a date field operator.
pub fn date_range(name: &str, value: &str) -> Option<(Bound<i64>, Bound<i64>)> {
    date_range_at(name, value, Local::now().date_naive())
}

pub fn date_range_at(name: &str, value: &str, today: NaiveDate) -> Option<(Bound<i64>, Bound<i64>)> {
    use Bound::*;
    if value.eq_ignore_ascii_case("none") {
        return Some((Included(0), Included(0)));
    }
    Some(match name {
        "after" => (Included(local_ts(date_span(value, today)?.0)), Unbounded),
        "before" => (Unbounded, Excluded(local_ts(date_span(value, today)?.0))),
        "date" | "on" | "year" | "month" => {
            let (a, b) = date_span(value, today)?;
            (Included(local_ts(a)), Excluded(local_ts(b)))
        }
        "newer_than" | "newer" => (Included(local_ts(age_start(value, today)?)), Unbounded),
        "older_than" | "older" => (Unbounded, Excluded(local_ts(age_start(value, today)?))),
        _ => return None,
    })
}

fn describe_range(lo: Bound<i64>, hi: Bound<i64>) -> String {
    let fmt = |t: i64| Local.timestamp_opt(t, 0).single().map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
    match (lo, hi) {
        (Bound::Included(a), Bound::Excluded(b)) => format!("{} to {}", fmt(a), fmt(b - 1)),
        (Bound::Included(a), Bound::Unbounded) => format!("from {}", fmt(a)),
        (Bound::Unbounded, Bound::Excluded(b)) => format!("before {}", fmt(b)),
        _ => String::new(),
    }
}

fn parse_size(v: &str) -> Option<u64> {
    let v = v.trim().to_lowercase();
    let v = v.trim_end_matches('b');
    let (num, mult) = match v.chars().last()? {
        'k' => (&v[..v.len() - 1], 1024u64),
        'm' => (&v[..v.len() - 1], 1024 * 1024),
        'g' => (&v[..v.len() - 1], 1024 * 1024 * 1024),
        _ => (v, 1),
    };
    num.trim().parse::<f64>().ok().map(|n| (n * mult as f64) as u64)
}

// ---------------------------------------------------------------- compiling

struct Ctx<'a> {
    index: &'a Index,
    f: &'a Fields,
}

impl Ctx<'_> {
    fn analyze(&self, tokenizer: &str, text: &str) -> Vec<String> {
        let Some(mut a) = self.index.tokenizers().get(tokenizer) else { return vec![] };
        let mut ts = a.token_stream(text);
        let mut out = Vec::new();
        while ts.advance() {
            out.push(ts.token().text.clone());
        }
        out
    }

    fn term(&self, field: Field, t: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(Term::from_field_text(field, t), IndexRecordOption::WithFreqs))
    }

    /// Term or phrase over one field, with the field's analyzer.
    fn text_in(&self, field: Field, tokenizer: &str, text: &str) -> Option<Box<dyn Query>> {
        let toks = self.analyze(tokenizer, text);
        match toks.len() {
            0 => None,
            1 => Some(self.term(field, &toks[0])),
            _ => Some(Box::new(PhraseQuery::new(toks.iter().map(|t| Term::from_field_text(field, t)).collect()))),
        }
    }

    fn boost(q: Box<dyn Query>, b: f32) -> Box<dyn Query> {
        if (b - 1.0).abs() < f32::EPSILON {
            q
        } else {
            Box::new(BoostQuery::new(q, b))
        }
    }

    fn any(qs: Vec<Box<dyn Query>>) -> Option<Box<dyn Query>> {
        match qs.len() {
            0 => None,
            1 => qs.into_iter().next(),
            _ => Some(Box::new(BooleanQuery::new(qs.into_iter().map(|q| (Occur::Should, q)).collect()))),
        }
    }

    fn regex(&self, field: Field, pattern: &str) -> Option<Box<dyn Query>> {
        RegexQuery::from_pattern(pattern, field).ok().map(|q| Box::new(q) as Box<dyn Query>)
    }

    fn wildcard_regex(w: &str) -> String {
        let mut r = String::new();
        for c in w.to_lowercase().chars() {
            match c {
                '*' => r.push_str(".*"),
                '?' => r.push('.'),
                c => r.push_str(&regex::escape(&c.to_string())),
            }
        }
        r
    }

    /// A free-text word across all searchable fields.
    fn word(&self, w: &str, fuzzy: bool) -> Option<Box<dyn Query>> {
        let f = self.f;
        // Lone punctuation ("-", "&", "#") carries no meaning; ignore it
        // rather than requiring it.
        if !w.chars().any(|c| c.is_alphanumeric()) {
            return None;
        }
        let mut qs: Vec<Box<dyn Query>> = Vec::new();
        if w.contains('*') || w.contains('?') {
            let core: String = w.chars().filter(|c| c.is_alphanumeric() || *c == '*' || *c == '?').collect();
            if core.chars().filter(|c| c.is_alphanumeric()).count() == 0 {
                return None;
            }
            let pat = Self::wildcard_regex(&core);
            for (field, b) in [(f.subject, 3.0), (f.body, 1.0), (f.att_text, 0.7), (f.from, 2.0), (f.to, 1.0), (f.att_names, 1.5)] {
                if let Some(q) = self.regex(field, &pat) {
                    qs.push(Self::boost(q, b));
                }
            }
            if let Some(q) = self.regex(f.raw, &Self::wildcard_regex(&clean_raw(w))) {
                qs.push(q);
            }
            return Self::any(qs);
        }
        let stems = self.analyze("en_stem", w);
        let plain = self.analyze("default", w);
        let special = w.chars().any(|c| !c.is_alphanumeric());
        if special {
            let r = clean_raw(w);
            if !r.is_empty() {
                qs.push(Self::boost(self.term(f.raw, &r), 2.5));
            }
        }
        if stems.len() == 1 {
            let s = &stems[0];
            qs.push(Self::boost(self.term(f.subject, s), 3.0));
            qs.push(self.term(f.body, s));
            qs.push(Self::boost(self.term(f.att_text, s), 0.7));
            let p = &plain[0];
            qs.push(Self::boost(self.term(f.from, p), 2.0));
            qs.push(self.term(f.to, p));
            qs.push(Self::boost(self.term(f.att_names, p), 1.5));
            qs.push(Self::boost(self.term(f.folder, p), 0.5));
            let n = s.chars().count();
            if fuzzy && n >= 4 && s.chars().all(|c| c.is_alphabetic()) {
                let d = if n >= 8 { 2 } else { 1 };
                for field in [f.subject, f.body, f.att_text] {
                    qs.push(Self::boost(
                        Box::new(FuzzyTermQuery::new(Term::from_field_text(field, s), d, true)),
                        0.3,
                    ));
                }
                // Names typed partially or misspelled.
                qs.push(Self::boost(Box::new(FuzzyTermQuery::new(Term::from_field_text(f.from, p), 1, true)), 0.5));
            }
        } else if stems.len() > 1 {
            // "e-mail", "user_id=99": phrase over the analysed tokens.
            for (field, tok, b) in [(f.subject, "en_stem", 3.0), (f.body, "en_stem", 1.0), (f.att_text, "en_stem", 0.7), (f.from, "default", 2.0), (f.to, "default", 1.0), (f.att_names, "default", 1.5)] {
                if let Some(q) = self.text_in(field, tok, w) {
                    qs.push(Self::boost(q, b));
                }
            }
        }
        Self::any(qs)
    }

    fn phrase(&self, p: &str) -> Option<Box<dyn Query>> {
        let f = self.f;
        let trimmed = p.trim();
        if let Some(prefix) = trimmed.strip_suffix('*') {
            let toks = self.analyze("default", prefix);
            if toks.is_empty() {
                return None;
            }
            let mut qs = Vec::new();
            for (field, b) in [(f.subject, 3.0), (f.body, 1.0), (f.att_text, 0.7)] {
                // Stem all but the last (prefix) token.
                let mut terms: Vec<Term> = Vec::new();
                for (i, t) in toks.iter().enumerate() {
                    let s = if i + 1 < toks.len() {
                        self.analyze("en_stem", t).into_iter().next().unwrap_or_else(|| t.clone())
                    } else {
                        t.clone()
                    };
                    terms.push(Term::from_field_text(field, &s));
                }
                let q: Box<dyn Query> = if terms.len() == 1 {
                    self.regex(field, &format!("{}.*", regex::escape(&toks[0])))?
                } else {
                    Box::new(PhrasePrefixQuery::new(terms))
                };
                qs.push(Self::boost(q, b));
            }
            return Self::any(qs);
        }
        let mut qs = Vec::new();
        for (field, tok, b) in [
            (f.subject, "en_stem", 3.0),
            (f.body, "en_stem", 1.0),
            (f.att_text, "en_stem", 0.7),
            (f.from, "default", 2.0),
            (f.to, "default", 1.0),
            (f.att_names, "default", 1.5),
        ] {
            if let Some(q) = self.text_in(field, tok, trimmed) {
                qs.push(Self::boost(q, b));
            }
        }
        if trimmed.split_whitespace().count() == 1 && trimmed.chars().any(|c| !c.is_alphanumeric()) {
            qs.push(Self::boost(self.term(f.raw, &clean_raw(trimmed)), 2.5));
        }
        Self::any(qs)
    }

    fn address(&self, fields: &[Field], v: &str) -> Option<Box<dyn Query>> {
        let mut qs = Vec::new();
        let toks = self.analyze("default", v);
        for &field in fields {
            if let Some(q) = self.text_in(field, "default", v) {
                qs.push(q);
            }
            // Prefix match on a single partial token: from:dan finds dana.
            if toks.len() == 1 {
                if let Some(q) = self.regex(field, &format!("{}.*", regex::escape(&toks[0]))) {
                    qs.push(Self::boost(q, 0.8));
                }
            }
        }
        Self::any(qs)
    }

    fn facet_term(&self, field: Field, path: &[String]) -> Box<dyn Query> {
        Box::new(TermQuery::new(Term::from_facet(field, &Facet::from_path(path)), IndexRecordOption::Basic))
    }

    fn field(&self, name: &str, value: &str) -> Option<Box<dyn Query>> {
        let f = self.f;
        let v = value.trim();
        Some(match name {
            "from" => self.address(&[f.from], v)?,
            "to" => self.address(&[f.to], v)?,
            "cc" | "bcc" => self.address(&[f.cc], v)?,
            "subject" => self.restricted(f.subject, v)?,
            "body" => self.restricted(f.body, v)?,
            "att" | "attachment" => self.restricted(f.att_text, v)?,
            "filename" => {
                let lv = v.to_lowercase();
                if let Some(ext) = lv.strip_prefix("*.") {
                    if ext.chars().all(|c| c.is_ascii_alphanumeric()) {
                        return Some(self.term(f.att_ext, ext));
                    }
                }
                if lv.contains('*') || lv.contains('?') {
                    let core: String = lv.split(['.', ' ', '-', '_']).find(|p| p.chars().any(|c| c.is_alphanumeric())).unwrap_or("").to_string();
                    let q = self.regex(f.att_names, &Self::wildcard_regex(&core));
                    return q;
                }
                self.text_in(f.att_names, "default", v)?
            }
            "has" => match v.to_lowercase().as_str() {
                "attachment" | "attachments" | "att" | "file" | "files" => {
                    Box::new(TermQuery::new(Term::from_field_u64(f.has_att, 1), IndexRecordOption::Basic))
                }
                "noattachment" | "noattachments" => {
                    Box::new(TermQuery::new(Term::from_field_u64(f.has_att, 0), IndexRecordOption::Basic))
                }
                other => self.term(f.att_ext, other),
            },
            "type" | "ext" => self.term(f.att_ext, v.trim_start_matches('.').to_lowercase().as_str()),
            "after" | "before" | "date" | "on" | "year" | "month" | "newer_than" | "older_than" | "newer" | "older" => {
                let (lo, hi) = date_range(name, v)?;
                Box::new(RangeQuery::new_i64_bounds("date".into(), lo, hi))
            }
            "larger" => Box::new(RangeQuery::new_u64_bounds("size".into(), Bound::Included(parse_size(v)?), Bound::Unbounded)),
            "smaller" => Box::new(RangeQuery::new_u64_bounds("size".into(), Bound::Unbounded, Bound::Excluded(parse_size(v)?))),
            "size" => {
                let n = parse_size(v.trim_start_matches(['>', '<']))?;
                if v.starts_with('<') {
                    Box::new(RangeQuery::new_u64_bounds("size".into(), Bound::Unbounded, Bound::Excluded(n)))
                } else {
                    Box::new(RangeQuery::new_u64_bounds("size".into(), Bound::Included(n), Bound::Unbounded))
                }
            }
            "folder" | "in" | "label" => self.text_in(f.folder, "default", v)?,
            "infolder" => {
                let parts: Vec<String> = v.split('/').filter(|s| !s.is_empty()).map(String::from).collect();
                if parts.is_empty() {
                    return None;
                }
                self.facet_term(f.f_folder, &parts)
            }
            "source" | "src" => self.facet_term(f.f_folder, &[v.to_string()]),
            "domain" => self.facet_term(f.f_sender, &[v.to_lowercase()]),
            "sender" => {
                let e = v.to_lowercase();
                let d = e.rsplit_once('@').map(|(_, d)| d.to_string()).unwrap_or_default();
                self.facet_term(f.f_sender, &[d, e])
            }
            "id" => self.term(f.id, v),
            "msgid" | "rfc822msgid" => self.term(f.msgid, v.trim_matches(['<', '>']).to_lowercase().as_str()),
            "thread" => self.term(f.thread, v),
            _ => return None,
        })
    }

    /// Text restricted to one stemmed field: words ANDed, typo tolerant.
    fn restricted(&self, field: Field, v: &str) -> Option<Box<dyn Query>> {
        if v.split_whitespace().count() > 1 {
            return self.text_in(field, "en_stem", v);
        }
        let stems = self.analyze("en_stem", v);
        if stems.len() != 1 {
            return self.text_in(field, "en_stem", v);
        }
        let s = &stems[0];
        let mut qs = vec![self.term(field, s)];
        if s.chars().count() >= 4 {
            qs.push(Self::boost(Box::new(FuzzyTermQuery::new(Term::from_field_text(field, s), 1, true)), 0.3));
        }
        if v.ends_with('*') {
            qs.push(self.regex(field, &format!("{}.*", regex::escape(s)))?);
        }
        Self::any(qs)
    }

    fn node(&self, n: &Node) -> Option<Box<dyn Query>> {
        match n {
            Node::Word(w) => self.word(w, true),
            Node::Phrase(p) => self.phrase(p),
            Node::Field { name, value } => match self.field(name, value) {
                Some(q) => Some(q),
                // An unparseable date or size is ignored (explain() says so)
                // rather than turning into a word that matches nothing.
                None if is_range_field(name) => None,
                None => self.word(&format!("{name}:{value}"), false),
            },
            Node::Any(ws) => Self::any(ws.iter().filter_map(|w| self.word(w, true)).collect()),
            Node::Or(v) => Self::any(v.iter().filter_map(|n| self.node(n)).collect()),
            Node::And(v) => Some(self.conj(v)),
            Node::Not(inner) => {
                let q = self.node(inner)?;
                Some(Box::new(BooleanQuery::new(vec![(Occur::Must, Box::new(AllQuery) as Box<dyn Query>), (Occur::MustNot, q)])))
            }
        }
    }

    fn conj(&self, items: &[Node]) -> Box<dyn Query> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for n in items {
            match n {
                Node::Not(inner) => {
                    if let Some(q) = self.node(inner) {
                        clauses.push((Occur::MustNot, q));
                    }
                }
                other => {
                    if let Some(q) = self.node(other) {
                        clauses.push((Occur::Must, q));
                    }
                }
            }
        }
        if !clauses.iter().any(|(o, _)| *o == Occur::Must) {
            clauses.push((Occur::Must, Box::new(AllQuery)));
        }
        if clauses.len() == 1 {
            return clauses.pop().unwrap().1;
        }
        Box::new(BooleanQuery::new(clauses))
    }
}

fn is_range_field(name: &str) -> bool {
    matches!(
        name,
        "after" | "before" | "date" | "on" | "year" | "month" | "newer_than" | "older_than" | "newer" | "older" | "larger" | "smaller" | "size"
    )
}

pub fn compile(p: &Parsed, index: &Index, f: &Fields) -> Box<dyn Query> {
    Ctx { index, f }.conj(&p.items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fields_phrases_or_not() {
        let p = parse(r#"invoice from:"Dana Smith" "order 44*" (a OR b) -newsletter"#);
        assert_eq!(p.items[0], Node::Word("invoice".into()));
        assert_eq!(p.items[1], Node::Field { name: "from".into(), value: "Dana Smith".into() });
        assert_eq!(p.items[2], Node::Phrase("order 44*".into()));
        assert_eq!(p.items[3], Node::Or(vec![Node::Word("a".into()), Node::Word("b".into())]));
        assert_eq!(p.items[4], Node::Not(Box::new(Node::Word("newsletter".into()))));
    }

    #[test]
    fn never_errors() {
        for q in ["\"unterminated", "((", ")))", "OR OR", "-", "from:", ":::", "a OR", "-(", "~", "“curly”"] {
            let _ = parse(q);
        }
        assert_eq!(parse("from:").items, vec![Node::Word("from:".into())]);
        assert_eq!(parse("http://x.com").items, vec![Node::Word("http://x.com".into())]);
    }

    #[test]
    fn or_chains_and_pipes() {
        let p = parse("a OR b | c d");
        assert_eq!(p.items.len(), 2);
        assert!(matches!(&p.items[0], Node::Or(v) if v.len() == 3));
    }

    #[test]
    fn exclusion_inside_or_is_allowed() {
        let p = parse("invoice OR -newsletter");
        assert!(matches!(&p.items[0], Node::Or(v) if matches!(v[1], Node::Not(_))));
    }

    #[test]
    fn dates() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 13).unwrap();
        assert_eq!(date_span("2019", today).unwrap().0, NaiveDate::from_ymd_opt(2019, 1, 1).unwrap());
        assert_eq!(date_span("2019-03", today).unwrap().1, NaiveDate::from_ymd_opt(2019, 4, 1).unwrap());
        assert_eq!(date_span("12/25/2020", today).unwrap().0, NaiveDate::from_ymd_opt(2020, 12, 25).unwrap());
        assert_eq!(date_span("last month", today).unwrap(), (NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(), NaiveDate::from_ymd_opt(2026, 11, 1).unwrap()));
        assert_eq!(date_span("last summer", today).unwrap().0, NaiveDate::from_ymd_opt(2026, 6, 1).unwrap());
        assert_eq!(date_span("december", today).unwrap().0, NaiveDate::from_ymd_opt(2025, 12, 1).unwrap());
        assert_eq!(date_span("march 2021", today).unwrap().0, NaiveDate::from_ymd_opt(2021, 3, 1).unwrap());
        assert!(date_span("gibberish", today).is_none());
        assert!(date_range_at("newer_than", "30d", today).is_some());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("5M"), Some(5 * 1024 * 1024));
        assert_eq!(parse_size("100kb"), Some(100 * 1024));
        assert_eq!(parse_size("2.5k"), Some(2560));
    }

    #[test]
    fn tilde_groups_merge() {
        let p = parse("~heater ~landlord");
        assert_eq!(p.items, vec![Node::Any(vec!["heater".into(), "landlord".into()])]);
    }
}
