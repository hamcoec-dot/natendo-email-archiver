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
use crate::profile::{self, Profile, Source};
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
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    serve_with(archive, opts, listener, open_browser).await
}

/// Serve on an already-bound local listener (the desktop shell binds port 0).
pub async fn serve_with(archive: Archive, opts: OpenOptions, listener: tokio::net::TcpListener, open_browser: bool) -> Result<()> {
    let port = listener.local_addr()?.port();
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
        .route("/api/message/:id/open", post(message_open))
        .route("/api/message/:id/att/:idx", get(attachment))
        .route("/api/thread/:id", get(thread))
        .route("/api/stats", get(stats))
        .route("/api/contacts", get(contacts))
        .route("/api/import", post(start_import))
        .route("/api/profiles", get(list_profiles).post(save_profile))
        .route("/api/profiles/:name", axum::routing::delete(delete_profile))
        .route("/api/profiles/:name/run", post(run_profile))
        .route("/api/outlook/folders", get(outlook_folders))
        .route("/api/detect", get(|| async { Json(tokio::task::spawn_blocking(crate::detect::detect).await.unwrap_or_default()) }))
        .route("/api/run", post(run_adhoc))
        .route("/api/jobs", get(jobs))
        .route("/api/export", post(export))
        .route("/api/delete", post(delete_msgs))
        .route("/api/compact", post(compact))
        .route("/api/backup", post(backup))
        .route("/api/export/imap", post(export_imap))
        .route("/api/export/smtp", post(export_smtp))
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

/// Open a message in the computer's default mail program (to reply,
/// forward or print there).
async fn message_open(State(st): State<S>, UrlPath(id): UrlPath<String>) -> ApiResult<Response> {
    if !valid_id(&id) {
        return Ok((StatusCode::BAD_REQUEST, "bad id").into_response());
    }
    let path = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        let raw = st.archive.lock().unwrap().read_raw(&id)?;
        let dir = std::env::temp_dir().join("natendo-open");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.eml", &id[..16]));
        std::fs::write(&path, raw)?;
        Ok(path)
    })
    .await??;
    open_url(&path.to_string_lossy());
    Ok(Json(json!({"opened": path.display().to_string()})).into_response())
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
    let source = match r.kind.as_str() {
        "eml" => Source::Eml { path, src: r.src.clone() },
        "mailstore" => Source::Mailstore { path, src: r.src.clone() },
        "mbox" => Source::Mbox { path, src: r.src.clone() },
        "pst" => Source::Pst { path, src: r.src.clone() },
        _ => return Ok((StatusCode::BAD_REQUEST, Json(json!({"error": "kind must be eml, mailstore, mbox or pst"})))),
    };
    let p = Profile { name: format!("import:{}", r.kind), source, window: Default::default(), password_env: None, last_run: None };
    spawn_job(&st, &r.kind, &r.path, p, None)
}

/// Run a profile as a background job holding the writer lock.
fn spawn_job(st: &S, kind: &str, label: &str, profile: Profile, password: Option<String>) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    if st.importing.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Ok((StatusCode::CONFLICT, Json(json!({"error": "an import is already running"}))));
    }
    let id = {
        let mut jobs = st.jobs.lock().unwrap();
        let id = jobs.len() as u64 + 1;
        jobs.push(Job { id, kind: kind.to_string(), path: label.to_string(), state: "running".into(), ..Default::default() });
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
            let mut w = writer(&st2)?;
            let stats = profile::run(
                &mut w,
                &profile,
                || password.clone().filter(|p| !p.is_empty()).ok_or_else(|| anyhow::anyhow!("a password is needed")),
                |_, s| update(&|j: &mut Job| j.stats = Some(s.clone())),
            )?;
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

fn writer(st: &AppState) -> Result<Archive> {
    let mut opts = st.opts.clone();
    opts.writer = true;
    opts.create = false;
    Archive::open(&st.root, &opts)
}

async fn list_profiles(State(st): State<S>) -> ApiResult<impl IntoResponse> {
    let all = profile::load(&st.root)?;
    Ok(Json(
        all.into_iter()
            .map(|p| json!({"name": p.name, "describe": p.describe(), "needs_password": p.needs_password() && p.password_env.as_deref().and_then(|v| std::env::var(v).ok()).is_none(), "last_run": p.last_run, "profile": p}))
            .collect::<Vec<_>>(),
    ))
}

async fn save_profile(State(st): State<S>, Json(p): Json<Profile>) -> ApiResult<impl IntoResponse> {
    if p.name.trim().is_empty() {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({"error": "name is required"}))));
    }
    if let Err(e) = p.source_window().validate().and_then(|_| p.window.validate()) {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({"error": format!("{e:#}")}))));
    }
    if st.importing.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok((StatusCode::CONFLICT, Json(json!({"error": "wait for the running import to finish"}))));
    }
    let st2 = st.clone();
    let d = p.describe();
    tokio::task::spawn_blocking(move || profile::upsert(&writer(&st2)?, p)).await??;
    Ok((StatusCode::OK, Json(json!({"saved": d}))))
}

async fn delete_profile(State(st): State<S>, UrlPath(name): UrlPath<String>) -> ApiResult<impl IntoResponse> {
    if st.importing.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok((StatusCode::CONFLICT, Json(json!({"error": "wait for the running import to finish"}))));
    }
    let st2 = st.clone();
    let removed = tokio::task::spawn_blocking(move || profile::remove(&writer(&st2)?, &name)).await??;
    Ok((StatusCode::OK, Json(json!({"removed": removed}))))
}

#[derive(Deserialize, Default)]
struct RunReq {
    #[serde(default)]
    password: Option<String>,
}

async fn run_profile(State(st): State<S>, UrlPath(name): UrlPath<String>, body: Option<Json<RunReq>>) -> ApiResult<impl IntoResponse> {
    let p = match profile::find(&st.root, &name) {
        Ok(p) => p,
        Err(e) => return Ok((StatusCode::NOT_FOUND, Json(json!({"error": format!("{e:#}")})))),
    };
    let pw = body.and_then(|b| b.0.password);
    let label = p.describe();
    spawn_job(&st, "profile", &label, p, pw)
}

#[derive(Deserialize)]
struct AdhocReq {
    profile: Profile,
    #[serde(default)]
    password: Option<String>,
}

/// Run an unsaved profile once (the "Archive now" button).
async fn run_adhoc(State(st): State<S>, Json(r): Json<AdhocReq>) -> ApiResult<impl IntoResponse> {
    if let Err(e) = r.profile.source_window().validate().and_then(|_| r.profile.window.validate()) {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({"error": format!("{e:#}")}))));
    }
    let label = r.profile.describe();
    spawn_job(&st, "archive", &label, r.profile, r.password)
}

async fn outlook_folders() -> ApiResult<impl IntoResponse> {
    let v = tokio::task::spawn_blocking(crate::import::outlook::list_folders).await??;
    Ok(Json(v.into_iter().map(|(store, path, count)| json!({"store": store, "path": path, "count": count})).collect::<Vec<_>>()))
}

async fn jobs(State(st): State<S>) -> impl IntoResponse {
    Json(st.jobs.lock().unwrap().clone())
}

#[derive(Deserialize)]
struct ExportReq {
    q: String,
    format: String,
    path: String,
}

/// Export the messages matching a query to a folder (.eml files) or a
/// single .mbox file on this computer.
#[derive(Deserialize)]
struct DeliverReq {
    q: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    imap: Option<crate::deliver::ImapTarget>,
    #[serde(default)]
    smtp: Option<crate::deliver::SmtpTarget>,
}

/// Copy the search results into an IMAP mailbox.
async fn export_imap(State(st): State<S>, Json(r): Json<DeliverReq>) -> ApiResult<impl IntoResponse> {
    let Some(mut t) = r.imap else {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({"error": "imap settings missing"}))));
    };
    t.password = r.password;
    let v = tokio::task::spawn_blocking(move || -> Result<crate::deliver::DeliverStats> {
        let ids = st.archive.lock().unwrap().index.all_ids(&r.q)?;
        // Reading needs only a short lock per message; use a separate reader.
        let a = Archive::open(&st.root, &st.opts)?;
        crate::deliver::to_imap(&a, &ids, &t, |_| {})
    })
    .await??;
    Ok((StatusCode::OK, Json(json!(v))))
}

/// Send the search results by email.
async fn export_smtp(State(st): State<S>, Json(r): Json<DeliverReq>) -> ApiResult<impl IntoResponse> {
    let Some(mut t) = r.smtp else {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({"error": "smtp settings missing"}))));
    };
    t.password = r.password;
    let v = tokio::task::spawn_blocking(move || -> Result<crate::deliver::DeliverStats> {
        let ids = st.archive.lock().unwrap().index.all_ids(&r.q)?;
        let a = Archive::open(&st.root, &st.opts)?;
        crate::deliver::to_smtp(&a, &ids, &t, |_| {})
    })
    .await??;
    Ok((StatusCode::OK, Json(json!(v))))
}

#[derive(Deserialize)]
struct DeleteReq {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    ids: Vec<String>,
}

/// Run a writer task while no import is running.
async fn with_writer<T: Send + 'static>(st: &S, f: impl FnOnce(&mut Archive) -> Result<T> + Send + 'static) -> ApiResult<std::result::Result<T, (StatusCode, String)>> {
    if st.importing.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Ok(Err((StatusCode::CONFLICT, "wait for the running import to finish".into())));
    }
    let st2 = st.clone();
    let r = tokio::task::spawn_blocking(move || -> Result<T> {
        let mut w = writer(&st2)?;
        let out = f(&mut w)?;
        drop(w);
        st2.archive.lock().unwrap().try_sync();
        Ok(out)
    })
    .await;
    st.importing.store(false, std::sync::atomic::Ordering::SeqCst);
    Ok(Ok(r??))
}

async fn delete_msgs(State(st): State<S>, Json(r): Json<DeleteReq>) -> ApiResult<impl IntoResponse> {
    let ids = match r.q.filter(|q| !q.trim().is_empty()) {
        Some(q) => {
            let st2 = st.clone();
            tokio::task::spawn_blocking(move || st2.archive.lock().unwrap().index.all_ids(&q)).await??
        }
        None => r.ids,
    };
    if ids.is_empty() {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({"error": "nothing selected"}))));
    }
    Ok(match with_writer(&st, move |w| w.delete_messages(&ids)).await? {
        Ok(n) => (StatusCode::OK, Json(json!({"deleted": n}))),
        Err((c, e)) => (c, Json(json!({"error": e}))),
    })
}

async fn compact(State(st): State<S>) -> ApiResult<impl IntoResponse> {
    Ok(match with_writer(&st, |w| w.compact(|_, _| {})).await? {
        Ok(r) => (StatusCode::OK, Json(json!(r))),
        Err((c, e)) => (c, Json(json!({"error": e}))),
    })
}

#[derive(Deserialize)]
struct BackupReq {
    path: String,
}

async fn backup(State(st): State<S>, Json(r): Json<BackupReq>) -> ApiResult<impl IntoResponse> {
    let dest = PathBuf::from(r.path.trim());
    if !dest.is_absolute() {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({"error": "Enter a full path, for example E:\\Mail backup"}))));
    }
    let root = st.root.clone();
    let rep = tokio::task::spawn_blocking(move || crate::backup::backup(&root, &dest, |_| {})).await??;
    Ok((StatusCode::OK, Json(json!(rep))))
}

async fn export(State(st): State<S>, Json(r): Json<ExportReq>) -> ApiResult<impl IntoResponse> {
    let out = PathBuf::from(r.path.trim());
    if r.path.trim().is_empty() || !out.is_absolute() {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({"error": "Enter a full path, for example C:\\Users\\me\\Desktop\\Exported mail"}))));
    }
    let v = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let a = st.archive.lock().unwrap();
        let ids = a.index.all_ids(&r.q)?;
        let (stats, dest) = if r.format == "mbox" {
            let file = if out.extension().is_some() { out.clone() } else { out.join("export.mbox") };
            (crate::export::to_mbox(&a, &ids, &file)?, file)
        } else if r.format == "msg" {
            (crate::export::to_msg_dir(&a, &ids, &out)?, out.clone())
        } else {
            (crate::export::to_eml_dir(&a, &ids, &out)?, out.clone())
        };
        Ok(json!({"written": stats.written, "failed": stats.failed, "path": dest.display().to_string()}))
    })
    .await??;
    Ok((StatusCode::OK, Json(v)))
}
