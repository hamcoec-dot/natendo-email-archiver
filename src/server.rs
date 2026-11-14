//! Local web UI + JSON API. Binds to 127.0.0.1 only and rejects requests
//! whose Host header is not local (DNS-rebinding guard). The same API can
//! back a Tauri shell later.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

use crate::archive::{Archive, ImportStats, OpenOptions};
use crate::parse;
use crate::query::Sort;

const INDEX_HTML: &str = include_str!("../ui/index.html");

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct Job {
    pub id: u64,
    pub kind: String,
    pub path: String,
    pub state: String, // running | done | failed
    pub stats: Option<ImportStats>,
    pub error: Option<String>,
}

pub struct AppState {
    pub archive: Mutex<Archive>,
    pub root: PathBuf,
    pub opts: OpenOptions,
    pub port: u16,
    pub jobs: Mutex<Vec<Job>>,
    /// Set while an import job owns the index writer; background sync waits.
    pub importing: std::sync::atomic::AtomicBool,
}

type S = Arc<AppState>;

struct ApiError(anyhow::Error);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{:#}", self.0)}))).into_response()
    }
}
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}
type ApiResult<T> = std::result::Result<T, ApiError>;

async fn host_guard(State(st): State<S>, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let ok = [format!("127.0.0.1:{}", st.port), format!("localhost:{}", st.port), format!("[::1]:{}", st.port)]
        .iter()
        .any(|h| h == host);
    if !ok {
        return (StatusCode::FORBIDDEN, "forbidden host").into_response();
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert("referrer-policy", "no-referrer".parse().unwrap());
    resp
}

pub async fn serve(archive: Archive, opts: OpenOptions, port: u16, open_browser: bool) -> Result<()> {
    let root = archive.root.clone();
    let st: S = Arc::new(AppState { archive: Mutex::new(archive), root, opts, port, jobs: Mutex::new(Vec::new()), importing: Default::default() });

    // Pick up changes written by other processes or machines (NAS use).
    let bg = st.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(20));
        if bg.importing.load(std::sync::atomic::Ordering::SeqCst) {
            continue;
        }
        if let Ok(mut a) = bg.archive.lock() {
            a.try_sync();
        }
    });

    let app = router(st.clone());
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let url = format!("http://127.0.0.1:{port}/");
    println!("Natendo Email Archiver running at {url}  (Ctrl+C to stop)");
    if open_browser {
        open_url(&url);
    }
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn router(st: S) -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/api/search", get(search))
        .route("/api/message/:id", get(message))
        .route("/api/message/:id/raw", get(message_raw))
        .route("/api/message/:id/att/:idx", get(attachment))
        .route("/api/thread/:id", get(thread))
        .route("/api/stats", get(stats))
        .route("/api/contacts", get(contacts))
        .route("/api/import", post(start_import))
        .route("/api/jobs", get(jobs))
        .layer(middleware::from_fn_with_state(st.clone(), host_guard))
        .with_state(st)
}

fn open_url(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
    sort: Option<Sort>,
    offset: Option<usize>,
    limit: Option<usize>,
}

async fn search(State(st): State<S>, Query(p): Query<SearchParams>) -> ApiResult<impl IntoResponse> {
    let res = tokio::task::spawn_blocking(move || {
        let a = st.archive.lock().unwrap();
        a.index.search(p.q.as_deref().unwrap_or(""), p.sort, p.offset.unwrap_or(0), p.limit.unwrap_or(50))
    })
    .await??;
    Ok(Json(res))
}

fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.chars().all(|c| c.is_ascii_hexdigit())
}

/// Make an HTML mail safe to show in a sandboxed iframe: no scripts (the
/// iframe sandbox blocks them), no remote loads unless allowed, inline cid:
/// images resolved to data URIs, links open outside.
fn safe_html(html: &str, raw: &[u8], allow_remote: bool) -> String {
    let mut out = html.to_string();
    // Resolve cid: references.
    if out.contains("cid:") {
        if let Some(msg) = mail_parser::MessageParser::default().parse(raw) {
            use mail_parser::MimeHeaders;
            for part in msg.parts.iter() {
                if let Some(cid) = part.content_id() {
                    let cid = cid.trim_matches(['<', '>']);
                    let needle = format!("cid:{cid}");
                    if out.contains(&needle) && part.contents().len() < 4 * 1024 * 1024 {
                        let ct = part
                            .content_type()
                            .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")))
                            .unwrap_or_else(|| "image/png".into());
                        let data = base64::engine::general_purpose::STANDARD.encode(part.contents());
                        out = out.replace(&needle, &format!("data:{ct};base64,{data}"));
                    }
                }
            }
        }
    }
    let img = if allow_remote { "data: https: http:" } else { "data:" };
    let head = format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src {img}; style-src 'unsafe-inline' {}; font-src data:\"><base target=\"_blank\">",
        if allow_remote { "https:" } else { "" }
    );
    format!("<!doctype html><html><head><meta charset=\"utf-8\">{head}</head><body>{out}</body></html>")
}

#[derive(Deserialize)]
struct MsgParams {
    remote: Option<bool>,
}

async fn message(State(st): State<S>, UrlPath(id): UrlPath<String>, Query(mp): Query<MsgParams>) -> ApiResult<Response> {
    if !valid_id(&id) {
        return Ok((StatusCode::BAD_REQUEST, "bad id").into_response());
    }
    let v = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let a = st.archive.lock().unwrap();
        let raw = a.read_raw(&id)?;
        let locs = a.catalog.locations(&id)?;
        let p = parse::parse(&raw);
        let thread_count = a.catalog.thread_ids(&p.thread_key())?.len();
        let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").or_else(|| raw.windows(2).position(|w| w == b"\n\n")).unwrap_or(raw.len().min(32768));
        Ok(json!({
            "id": id,
            "subject": p.subject,
            "from": p.from, "to": p.to, "cc": p.cc, "bcc": p.bcc,
            "date": p.date,
            "message_id": p.message_id,
            "text": p.body_text,
            "html": p.body_html.as_deref().map(|h| safe_html(h, &raw, mp.remote.unwrap_or(false))),
            "attachments": p.attachments.iter().filter(|a| !(a.inline && a.content_id.is_some())).collect::<Vec<_>>(),
            "locations": locs.iter().map(|(s, f)| json!({"src": s, "folder": f})).collect::<Vec<_>>(),
            "thread": p.thread_key(),
            "thread_count": thread_count,
            "size": raw.len(),
            "headers": String::from_utf8_lossy(&raw[..header_end]),
        }))
    })
    .await??;
    Ok(Json(v).into_response())
}

async fn message_raw(State(st): State<S>, UrlPath(id): UrlPath<String>) -> ApiResult<Response> {
    if !valid_id(&id) {
        return Ok((StatusCode::BAD_REQUEST, "bad id").into_response());
    }
    let raw = tokio::task::spawn_blocking(move || st.archive.lock().unwrap().read_raw(&id)).await??;
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, "message/rfc822".parse()?);
    h.insert(header::CONTENT_DISPOSITION, "attachment; filename=\"message.eml\"".parse()?);
    Ok((h, raw).into_response())
}

async fn attachment(State(st): State<S>, UrlPath((id, idx)): UrlPath<(String, usize)>) -> ApiResult<Response> {
    if !valid_id(&id) {
        return Ok((StatusCode::BAD_REQUEST, "bad id").into_response());
    }
    let r = tokio::task::spawn_blocking(move || -> Result<Option<(String, String, Vec<u8>)>> {
        let raw = st.archive.lock().unwrap().read_raw(&id)?;
        Ok(parse::attachment_bytes(&raw, idx))
    })
    .await??;
    let Some((name, _ct, data)) = r else { return Ok((StatusCode::NOT_FOUND, "no such attachment").into_response()) };
    let safe: String = name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let mut h = HeaderMap::new();
    // Always download, never render inline (attachments are untrusted).
    h.insert(header::CONTENT_TYPE, "application/octet-stream".parse()?);
    h.insert(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{safe}\"").parse()?);
    Ok((h, data).into_response())
}

async fn thread(State(st): State<S>, UrlPath(id): UrlPath<String>) -> ApiResult<impl IntoResponse> {
    let v = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let a = st.archive.lock().unwrap();
        let raw = a.read_raw(&id)?;
        let key = parse::parse(&raw).thread_key();
        let mut hits = Vec::new();
        for tid in a.catalog.thread_ids(&key)? {
            if let Some(h) = a.index.get(&tid)? {
                hits.push(h);
            }
        }
        Ok(json!({ "hits": hits }))
    })
    .await??;
    Ok(Json(v))
}

async fn stats(State(st): State<S>) -> ApiResult<impl IntoResponse> {
    let v = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let a = st.archive.lock().unwrap();
        Ok(json!({
            "stats": a.catalog.stats()?,
            "folders": a.catalog.folders()?,
            "imports": a.catalog.imports()?,
            "indexed": a.index.doc_count(),
            "archive": a.root.display().to_string(),
            "cache": a.cache.display().to_string(),
            "version": env!("CARGO_PKG_VERSION"),
        }))
    })
    .await??;
    Ok(Json(v))
}

async fn contacts(State(st): State<S>, Query(p): Query<HashMap<String, String>>) -> ApiResult<impl IntoResponse> {
    let q = p.get("q").cloned().unwrap_or_default();
    let v = tokio::task::spawn_blocking(move || st.archive.lock().unwrap().catalog.contacts(&q, 12)).await??;
    Ok(Json(v.into_iter().map(|(e, n, c)| json!({"email": e, "name": n, "count": c})).collect::<Vec<_>>()))
}

#[derive(Deserialize)]
struct ImportReq {
    kind: String,
    path: String,
    src: Option<String>,
}

async fn start_import(State(st): State<S>, Json(r): Json<ImportReq>) -> ApiResult<impl IntoResponse> {
    let path = PathBuf::from(&r.path);
    if !path.exists() {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({"error": "path does not exist"}))));
    }
    if !matches!(r.kind.as_str(), "eml" | "mailstore" | "mbox" | "pst") {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({"error": "kind must be eml, mailstore, mbox or pst"}))));
    }
    if st.importing.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Ok((StatusCode::CONFLICT, Json(json!({"error": "an import is already running"}))));
    }
    let id = {
        let mut jobs = st.jobs.lock().unwrap();
        let id = jobs.len() as u64 + 1;
        jobs.push(Job { id, kind: r.kind.clone(), path: r.path.clone(), state: "running".into(), ..Default::default() });
        id
    };
    let st2 = st.clone();
    std::thread::spawn(move || {
        let update = |f: &dyn Fn(&mut Job)| {
            if let Some(j) = st2.jobs.lock().unwrap().iter_mut().find(|j| j.id == id) {
                f(j);
            }
        };
        let res = (|| -> Result<ImportStats> {
            let mut opts = st2.opts.clone();
            opts.writer = true;
            opts.create = false;
            let mut w = Archive::open(&st2.root, &opts)?;
            let prog = |s: &ImportStats| update(&|j: &mut Job| j.stats = Some(s.clone()));
            let src = r.src.clone();
            let stats = match r.kind.as_str() {
                "eml" => crate::import::eml::import(&mut w, &path, src.as_deref().unwrap_or("files"), crate::import::eml::Flavor::Files, prog)?,
                "mailstore" => crate::import::eml::import(&mut w, &path, src.as_deref().unwrap_or("mailstore"), crate::import::eml::Flavor::MailStore, prog)?,
                "pst" => crate::import::pst::import(&mut w, &path, src.as_deref().unwrap_or("outlook"), prog)?,
                _ => crate::import::mbox::import(&mut w, &path, src.as_deref().unwrap_or("mbox"), prog)?,
            };
            w.commit()?;
            drop(w);
            st2.archive.lock().unwrap().try_sync();
            Ok(stats)
        })();
        st2.importing.store(false, std::sync::atomic::Ordering::SeqCst);
        match res {
            Ok(s) => update(&|j: &mut Job| {
                j.state = "done".into();
                j.stats = Some(s.clone());
            }),
            Err(e) => update(&|j: &mut Job| {
                j.state = "failed".into();
                j.error = Some(format!("{e:#}"));
            }),
        }
    });
    Ok((StatusCode::OK, Json(json!({"job": id}))))
}

async fn jobs(State(st): State<S>) -> impl IntoResponse {
    Json(st.jobs.lock().unwrap().clone())
}
