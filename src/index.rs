//! Full-text index (Tantivy). Disposable: rebuilt from the archive folder.

use std::path::Path;

use anyhow::Result;
use tantivy::collector::{Count, FacetCollector, TopDocs};
use tantivy::query::{Query, TermQuery};
use tantivy::schema::{
    Facet, FacetOptions, Field, IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions, Value,
    STORED, STRING,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{LowerCaser, TextAnalyzer, WhitespaceTokenizer};
use tantivy::{DocAddress, Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term};

use crate::parse::ParsedMessage;
use crate::query::{self, Sort};

/// Bump when the schema or analysis changes; a mismatch triggers a rebuild.
pub const INDEX_VERSION: &str = "2";

/// Body text indexed per message (attachments have their own field).
const MAX_BODY: usize = 512 * 1024;

#[derive(Clone)]
pub struct Fields {
    pub id: Field,
    pub subject: Field,
    pub body: Field,
    pub from: Field,
    pub to: Field,
    pub cc: Field,
    pub att_names: Field,
    pub att_text: Field,
    pub folder: Field,
    pub raw: Field,
    pub date: Field,
    pub size: Field,
    pub has_att: Field,
    pub archived: Field,
    pub priority: Field,
    pub att_ext: Field,
    pub thread: Field,
    pub msgid: Field,
    pub f_sender: Field,
    pub f_date: Field,
    pub f_folder: Field,
    pub f_type: Field,
}

fn text(tokenizer: &str, stored: bool) -> TextOptions {
    let o = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    if stored {
        o.set_stored()
    } else {
        o
    }
}

pub fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let num = NumericOptions::default().set_indexed().set_fast().set_stored();
    let f = Fields {
        id: b.add_text_field("id", STRING | STORED),
        subject: b.add_text_field("subject", text("en_stem", true)),
        body: b.add_text_field("body", text("en_stem", true)),
        from: b.add_text_field("from", text("default", true)),
        to: b.add_text_field("to", text("default", true)),
        cc: b.add_text_field("cc", text("default", false)),
        att_names: b.add_text_field("att_names", text("default", true)),
        att_text: b.add_text_field("att_text", text("en_stem", true)),
        folder: b.add_text_field("folder", text("default", true)),
        raw: b.add_text_field("raw", text("raw", false)),
        date: b.add_i64_field("date", num.clone()),
        size: b.add_u64_field("size", num.clone()),
        has_att: b.add_u64_field("has_att", num.clone()),
        archived: b.add_i64_field("archived", num.clone()),
        priority: b.add_u64_field("priority", num),
        att_ext: b.add_text_field("att_ext", STRING),
        thread: b.add_text_field("thread", STRING | STORED),
        msgid: b.add_text_field("msgid", STRING),
        f_sender: b.add_facet_field("f_sender", FacetOptions::default()),
        f_date: b.add_facet_field("f_date", FacetOptions::default()),
        f_folder: b.add_facet_field("f_folder", FacetOptions::default()),
        f_type: b.add_facet_field("f_type", FacetOptions::default()),
    };
    (b.build(), f)
}

fn register_tokenizers(index: &Index) {
    index.tokenizers().register(
        "raw",
        TextAnalyzer::builder(WhitespaceTokenizer::default()).filter(LowerCaser).build(),
    );
}

/// Turn text into whitespace-separated "raw" tokens that keep inner
/// punctuation (#4471, C++, user_id=99, a.b@c.com) but drop sentence
/// punctuation at the edges.
pub fn raw_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len() / 2);
    for w in s.split_whitespace() {
        let t = clean_raw(w);
        if t.chars().any(|c| !c.is_alphanumeric()) && t.chars().any(|c| c.is_alphanumeric()) {
            // Only tokens with special characters are worth a raw entry;
            // plain words are already in the analysed fields.
            out.push_str(&t);
            out.push(' ');
        }
    }
    out
}

pub fn clean_raw(w: &str) -> String {
    w.trim_start_matches(|c: char| matches!(c, '(' | '[' | '{' | '"' | '\'' | '<' | '“' | '‘'))
        .trim_end_matches(|c: char| matches!(c, ')' | ']' | '}' | '"' | '\'' | '>' | ',' | '.' | ';' | ':' | '!' | '?' | '”' | '’'))
        .to_lowercase()
}

pub fn date_facet(ts: i64) -> Facet {
    use chrono::{Datelike, TimeZone};
    match chrono::Local.timestamp_opt(ts, 0).single() {
        Some(d) if ts > 0 => Facet::from_path([format!("{:04}", d.year()), format!("{:02}", d.month())]),
        _ => Facet::from_path(["unknown"]),
    }
}

pub fn folder_facet(src: &str, folder: &str) -> Facet {
    let mut parts = vec![src.to_string()];
    parts.extend(folder.split(['/', '\\']).filter(|s| !s.is_empty()).map(String::from));
    Facet::from_path(parts)
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub struct SearchIndex {
    pub index: Index,
    pub reader: IndexReader,
    pub f: Fields,
    writer: Option<IndexWriter>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Hit {
    pub id: String,
    pub score: f32,
    pub subject: String,
    pub from: String,
    pub to: String,
    pub date: i64,
    pub size: u64,
    pub folders: Vec<String>,
    pub attachments: Vec<String>,
    pub thread: String,
    /// HTML snippet with <b> highlights (escaped).
    pub snippet: String,
    /// Snippet from attachment text, when the match was inside an attachment.
    pub att_snippet: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FacetBucket {
    pub label: String,
    /// Query clause that narrows to this bucket.
    pub filter: String,
    pub count: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct Facets {
    pub sender: Vec<FacetBucket>,
    pub date: Vec<FacetBucket>,
    pub folder: Vec<FacetBucket>,
    pub r#type: Vec<FacetBucket>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    pub total: usize,
    pub hits: Vec<Hit>,
    pub facets: Facets,
    pub took_ms: u128,
    /// How the query was understood, for the UI's "searching for" line.
    pub explain: String,
    /// True when nothing matched all words and the results match any of
    /// them instead.
    pub relaxed: bool,
}

impl SearchIndex {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let (schema, f) = build_schema();
        let mut existing = None;
        if dir.join("meta.json").exists() {
            let old = Index::open_in_dir(dir)?;
            // An older index format lacks newer fields: start a fresh index
            // (the archive notices the version change and rebuilds it).
            let same = schema.fields().all(|(_, e)| old.schema().get_field(e.name()).is_ok());
            if same {
                existing = Some(old);
            } else {
                drop(old);
                std::fs::remove_dir_all(dir)?;
                std::fs::create_dir_all(dir)?;
            }
        }
        let index = match existing {
            Some(i) => i,
            None => Index::create_in_dir(dir, schema)?,
        };
        register_tokenizers(&index);
        let reader = index.reader_builder().reload_policy(ReloadPolicy::OnCommitWithDelay).try_into()?;
        Ok(Self { index, reader, f, writer: None })
    }

    fn writer(&mut self) -> Result<&mut IndexWriter> {
        if self.writer.is_none() {
            self.writer = Some(self.index.writer(128_000_000)?);
        }
        Ok(self.writer.as_mut().unwrap())
    }

    /// `archived`: when the message first entered the archive (Unix seconds).
    pub fn build_doc(&self, id: &str, size: usize, p: &ParsedMessage, locations: &[(String, String)], archived: i64) -> TantivyDocument {
        let f = &self.f;
        let mut d = TantivyDocument::new();
        d.add_text(f.id, id);
        d.add_text(f.subject, &p.subject);
        d.add_text(f.body, truncate(&p.body_text, MAX_BODY));
        for a in &p.from {
            d.add_text(f.from, a.display());
            if !a.email.is_empty() {
                d.add_facet(f.f_sender, Facet::from_path([a.domain(), a.email.clone()]));
            }
        }
        for a in p.to.iter().chain(&p.cc).chain(&p.bcc) {
            d.add_text(f.to, a.display());
        }
        for a in &p.cc {
            d.add_text(f.cc, a.display());
        }
        let mut raw = raw_tokens(&p.subject);
        raw.push_str(&raw_tokens(truncate(&p.body_text, MAX_BODY)));
        let mut real_atts = 0;
        for a in &p.attachments {
            d.add_text(f.att_names, &a.name);
            raw.push_str(&clean_raw(&a.name));
            raw.push(' ');
            if let Some(t) = &a.text {
                d.add_text(f.att_text, t);
            }
            if !a.inline || a.content_id.is_none() {
                real_atts += 1;
                let ext = a.extension();
                if !ext.is_empty() {
                    d.add_text(f.att_ext, &ext);
                    d.add_facet(f.f_type, Facet::from_path([ext]));
                }
            }
        }
        d.add_text(f.raw, raw);
        for (src, folder) in locations {
            d.add_text(f.folder, format!("{src}:{folder}"));
            d.add_facet(f.f_folder, folder_facet(src, folder));
        }
        d.add_i64(f.date, p.date);
        d.add_u64(f.size, size as u64);
        d.add_u64(f.has_att, (real_atts > 0) as u64);
        d.add_i64(f.archived, archived);
        d.add_u64(f.priority, if p.priority == 0 { 3 } else { p.priority as u64 });
        d.add_facet(f.f_date, date_facet(p.date));
        let tk = p.thread_key();
        d.add_text(f.thread, if tk.is_empty() { id } else { tk.as_str() });
        if !p.message_id.is_empty() {
            d.add_text(f.msgid, p.message_id.to_lowercase());
        }
        d
    }

    /// Add (or replace) a message document.
    pub fn upsert(&mut self, doc: TantivyDocument, id: &str, replace: bool) -> Result<()> {
        let idf = self.f.id;
        let w = self.writer()?;
        if replace {
            w.delete_term(Term::from_field_text(idf, id));
        }
        w.add_document(doc)?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        if let Some(w) = self.writer.as_mut() {
            w.commit()?;
            self.reader.reload()?;
        }
        Ok(())
    }

    /// Release the writer (and Tantivy's directory lock) so another local
    /// process can index.
    pub fn close_writer(&mut self) -> Result<()> {
        self.commit()?;
        if let Some(w) = self.writer.take() {
            w.wait_merging_threads()?;
        }
        Ok(())
    }

    pub fn clear(&mut self) -> Result<()> {
        let w = self.writer()?;
        w.delete_all_documents()?;
        Ok(())
    }

    pub fn doc_count(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    fn stored_str(doc: &TantivyDocument, field: Field) -> String {
        doc.get_first(field).and_then(|v| v.as_str()).unwrap_or("").to_string()
    }

    fn stored_all(doc: &TantivyDocument, field: Field) -> Vec<String> {
        doc.get_all(field).filter_map(|v| v.as_str().map(String::from)).collect()
    }

    pub fn search(&self, q: &str, sort: Option<Sort>, offset: usize, limit: usize) -> Result<SearchResult> {
        let t0 = std::time::Instant::now();
        let parsed = query::parse(q);
        let mut r = self.search_parsed(&parsed, sort, offset, limit)?;
        if r.total == 0 {
            if let Some(relaxed) = parsed.relaxed() {
                let mut r2 = self.search_parsed(&relaxed, sort, offset, limit)?;
                if r2.total > 0 {
                    r2.relaxed = true;
                    r2.explain = format!("no message has all of these; showing any of them: {}", r2.explain);
                    r = r2;
                }
            }
        }
        r.took_ms = t0.elapsed().as_millis();
        Ok(r)
    }

    fn search_parsed(&self, parsed: &query::Parsed, sort: Option<Sort>, offset: usize, limit: usize) -> Result<SearchResult> {
        let t0 = std::time::Instant::now();
        let searcher = self.reader.searcher();
        let parsed = parsed.clone();
        let compiled = query::compile(&parsed, &self.index, &self.f);
        let sort = sort.unwrap_or(if parsed.has_text_terms() { Sort::Relevance } else { Sort::Newest });
        let limit = limit.clamp(1, 1000);

        let mut fc_sender = FacetCollector::for_field("f_sender");
        let mut fc_date = FacetCollector::for_field("f_date");
        let mut fc_folder = FacetCollector::for_field("f_folder");
        let mut fc_type = FacetCollector::for_field("f_type");
        for fc in [&mut fc_sender, &mut fc_date, &mut fc_folder, &mut fc_type] {
            fc.add_facet(Facet::root());
        }

        let (addrs, total, facets) = match sort {
            Sort::Relevance => {
                // BM25 with a gentle recency prior: newer mail wins ties.
                let now = chrono::Utc::now().timestamp();
                let top = TopDocs::with_limit(limit).and_offset(offset).tweak_score(
                    move |seg: &tantivy::SegmentReader| {
                        let dates = seg.fast_fields().i64("date").ok().map(|c| c.first_or_default_col(0));
                        move |doc: tantivy::DocId, score: f32| {
                            let d = dates.as_ref().map(|c| c.get_val(doc)).unwrap_or(0);
                            let age_years = ((now - d).max(0) as f32) / 31_557_600.0;
                            score * (1.0 + 0.25 * (-age_years / 3.0).exp())
                        }
                    },
                );
                let (top, total, (s, d, fo, ty)) =
                    searcher.search(&compiled, &(top, Count, (fc_sender, fc_date, fc_folder, fc_type)))?;
                (top.into_iter().map(|(s, a)| (s, a)).collect::<Vec<_>>(), total, (s, d, fo, ty))
            }
            Sort::Newest | Sort::Oldest => {
                let order = if sort == Sort::Newest { tantivy::Order::Desc } else { tantivy::Order::Asc };
                let top = TopDocs::with_limit(limit).and_offset(offset).order_by_fast_field::<i64>("date", order);
                let (top, total, (s, d, fo, ty)) =
                    searcher.search(&compiled, &(top, Count, (fc_sender, fc_date, fc_folder, fc_type)))?;
                (top.into_iter().map(|(_, a)| (0.0f32, a)).collect::<Vec<_>>(), total, (s, d, fo, ty))
            }
        };

        let mut body_snip = SnippetGenerator::create(&searcher, &*compiled, self.f.body)?;
        body_snip.set_max_num_chars(220);
        let mut att_snip = SnippetGenerator::create(&searcher, &*compiled, self.f.att_text)?;
        att_snip.set_max_num_chars(160);

        let mut hits = Vec::with_capacity(addrs.len());
        for (score, addr) in addrs {
            hits.push(self.hit(&searcher, addr, score, Some((&body_snip, &att_snip)))?);
        }

        let facets = Facets {
            sender: self.facet_buckets(&searcher, &compiled, facets.0, "f_sender"),
            date: self.facet_buckets(&searcher, &compiled, facets.1, "f_date"),
            folder: self.facet_buckets(&searcher, &compiled, facets.2, "f_folder"),
            r#type: self.facet_buckets(&searcher, &compiled, facets.3, "f_type"),
        };
        Ok(SearchResult { total, hits, facets, took_ms: t0.elapsed().as_millis(), explain: parsed.explain(), relaxed: false })
    }

    fn hit(
        &self,
        searcher: &Searcher,
        addr: DocAddress,
        score: f32,
        snip: Option<(&SnippetGenerator, &SnippetGenerator)>,
    ) -> Result<Hit> {
        let f = &self.f;
        let doc: TantivyDocument = searcher.doc(addr)?;
        let (snippet, att_snippet) = match snip {
            Some((b, a)) => {
                let mut s = b.snippet_from_doc(&doc).to_html();
                if s.is_empty() {
                    let body = Self::stored_str(&doc, f.body);
                    s = html_escape::encode_text(truncate(body.trim(), 200)).to_string();
                }
                (s, a.snippet_from_doc(&doc).to_html())
            }
            None => (String::new(), String::new()),
        };
        Ok(Hit {
            id: Self::stored_str(&doc, f.id),
            score,
            subject: Self::stored_str(&doc, f.subject),
            from: Self::stored_all(&doc, f.from).join(", "),
            to: Self::stored_all(&doc, f.to).join(", "),
            date: doc.get_first(f.date).and_then(|v| v.as_i64()).unwrap_or(0),
            size: doc.get_first(f.size).and_then(|v| v.as_u64()).unwrap_or(0),
            folders: Self::stored_all(&doc, f.folder),
            attachments: Self::stored_all(&doc, f.att_names),
            thread: Self::stored_str(&doc, f.thread),
            snippet,
            att_snippet,
        })
    }

    /// Facet counts. A level with a single bucket is drilled into (one
    /// domain -> its senders, one year -> its months), so facets stay useful
    /// as the query narrows.
    fn facet_buckets(
        &self,
        searcher: &Searcher,
        q: &dyn Query,
        counts: tantivy::collector::FacetCounts,
        field: &str,
    ) -> Vec<FacetBucket> {
        let mut level: Vec<(Facet, u64)> = counts.get("/").map(|(f, c)| (f.clone(), c)).collect();
        for _ in 0..4 {
            if level.len() != 1 {
                break;
            }
            let parent = level[0].0.clone();
            let mut fc = FacetCollector::for_field(field);
            fc.add_facet(parent.clone());
            let Ok(c) = searcher.search(q, &fc) else { break };
            let next: Vec<(Facet, u64)> = c.get(&parent.to_path_string()).map(|(f, c)| (f.clone(), c)).collect();
            if next.is_empty() {
                break;
            }
            level = next;
        }
        level.sort_by(|a, b| {
            if field == "f_date" {
                b.0.cmp(&a.0)
            } else {
                b.1.cmp(&a.1).then(a.0.cmp(&b.0))
            }
        });
        level.truncate(if field == "f_date" { 40 } else { 25 });
        level
            .into_iter()
            .map(|(facet, count)| {
                let path = facet.to_path();
                let (label, filter) = facet_filter(field, &path);
                FacetBucket { label, filter, count }
            })
            .collect()
    }

    /// Fetch one hit (no snippet) by message id.
    pub fn get(&self, id: &str) -> Result<Option<Hit>> {
        let searcher = self.reader.searcher();
        let q = TermQuery::new(Term::from_field_text(self.f.id, id), IndexRecordOption::Basic);
        let top = searcher.search(&q, &TopDocs::with_limit(1))?;
        match top.first() {
            Some((_, addr)) => Ok(Some(self.hit(&searcher, *addr, 0.0, None)?)),
            None => Ok(None),
        }
    }

    /// All ids matching a query, in date order (for export).
    pub fn all_ids(&self, q: &str) -> Result<Vec<String>> {
        let searcher = self.reader.searcher();
        let parsed = query::parse(q);
        let compiled = query::compile(&parsed, &self.index, &self.f);
        let n = searcher.num_docs() as usize;
        let top = TopDocs::with_limit(n.max(1)).order_by_fast_field::<i64>("date", tantivy::Order::Asc);
        let res = searcher.search(&compiled, &top)?;
        let mut out = Vec::with_capacity(res.len());
        for (_, addr) in res {
            let doc: TantivyDocument = searcher.doc(addr)?;
            out.push(Self::stored_str(&doc, self.f.id));
        }
        Ok(out)
    }
}

fn quote(s: &str) -> String {
    if s.chars().any(|c| c.is_whitespace() || c == '"' || c == '(' || c == ')') {
        format!("\"{}\"", s.replace('"', ""))
    } else {
        s.to_string()
    }
}

/// Label and query clause for a facet path.
fn facet_filter(field: &str, path: &[&str]) -> (String, String) {
    match field {
        "f_sender" => match path {
            [domain] => (domain.to_string(), format!("domain:{}", quote(domain))),
            [_, email, ..] => (email.to_string(), format!("sender:{}", quote(email))),
            _ => (String::new(), String::new()),
        },
        "f_date" => match path {
            ["unknown", ..] => ("No date".into(), "date:none".into()),
            [y] => (y.to_string(), format!("year:{y}")),
            [y, m, ..] => {
                let name = chrono::NaiveDate::from_ymd_opt(y.parse().unwrap_or(2000), m.parse().unwrap_or(1), 1)
                    .map(|d| d.format("%b %Y").to_string())
                    .unwrap_or_else(|| format!("{y}-{m}"));
                (name, format!("month:{y}-{m}"))
            }
            _ => (String::new(), String::new()),
        },
        "f_folder" => {
            let label = path.join("/");
            (label.clone(), format!("infolder:{}", quote(&label)))
        }
        "f_type" => {
            let t = path.first().copied().unwrap_or("");
            (t.to_uppercase(), format!("type:{t}"))
        }
        _ => (String::new(), String::new()),
    }
}
