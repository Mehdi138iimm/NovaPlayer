/* v1.10.1: SEARCH & DOWNLOAD page (Rust side). v1.11.0: also carries the deep-search
   JSON calls (Google Suggest, iTunes, Deezer, Audius, Archive) so the page needs no CORS
   and the CSP stays tight.
   The page searches free music catalogs (Audius, Internet Archive) and suggestions
   (iTunes). Everything network-heavy happens here in Rust: no CORS, no WebView quirks,
   real progress events and cancel support while a song downloads.
   Rules: https only, no local/private hosts, HTML/JSON bodies are refused as "audio",
   and every response has a size cap.
   v1.10.1 fixes: cancel now works while connecting / while the server is silent (it was
   only checked when a chunk arrived), a stalled download fails after 45s instead of
   hanging for up to an hour, a cancel sent right at the start is no longer lost, and
   finished job ids no longer pile up in the cancel set. */
use serde::Serialize;
use std::{collections::HashSet, future::Future, sync::Mutex, time::{Duration, Instant}};
use tauri::Emitter;

static CANCELLED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
const TICK: Duration = Duration::from_millis(250);
const STALL: Duration = Duration::from_secs(45);

fn is_cancelled(id: &str) -> bool {
    CANCELLED.lock().ok().and_then(|g| g.as_ref().map(|s| s.contains(id))).unwrap_or(false)
}
fn clear_cancel(id: &str) {
    if let Ok(mut g) = CANCELLED.lock() { if let Some(s) = g.as_mut() { s.remove(id); } }
}

fn check_url(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url.trim()).map_err(|e| e.to_string())?;
    if parsed.scheme() != "https" { return Err("only https links are allowed".into()); }
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let host_trim = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local")
        || host_trim.parse::<std::net::IpAddr>().is_ok() {
        return Err("host not allowed".into());
    }
    Ok(parsed)
}

fn client(total_secs: u64) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("NOVA-Player/", env!("CARGO_PKG_VERSION"), " (https://github.com/Mehdi138iimm/NovaPlayer)"))
        .timeout(Duration::from_secs(total_secs))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| e.to_string())
}

/* Await `fut` but wake up every TICK to honour search_cancel(id) and give up after
   STALL of silence. The future is pinned and polled again, never dropped mid-way. */
async fn watch<T, F: Future<Output = T>>(id: &str, fut: F) -> Result<T, String> {
    let mut fut = std::pin::pin!(fut);
    let started = Instant::now();
    loop {
        if is_cancelled(id) { return Err("cancelled".into()); }
        match tokio::time::timeout(TICK, &mut fut).await {
            Ok(v) => return Ok(v),
            Err(_) => if started.elapsed() >= STALL { return Err("timed out (no data from server)".into()); },
        }
    }
}

/* JSON/text GET for the search providers (fallback when the page's own fetch fails). */
#[tauri::command]
pub async fn search_http_get(url: String) -> Result<String, String> {
    let parsed = check_url(&url)?;
    let res = client(20)?.get(parsed).header("Accept", "application/json, text/javascript, text/plain, */*")
        .header("Accept-Language", "fa-IR,fa;q=0.9,en;q=0.8")
        .send().await.map_err(|e| e.to_string())?;
    let status = res.status();
    if !status.is_success() { return Err(format!("http {}", status.as_u16())); }
    if res.content_length().unwrap_or(0) > 8 * 1024 * 1024 { return Err("response too large".into()); }
    let body = res.text().await.map_err(|e| e.to_string())?;
    if body.len() > 8 * 1024 * 1024 { return Err("response too large".into()); }
    Ok(body)
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Progress { id: String, received: u64, total: u64, done: bool }

/* Binary download (songs and cover art). Streams chunk by chunk, emits
   `nova-search-progress` to the main window ~6 times a second and honours
   search_cancel(id). Returned as a raw ArrayBuffer (binary IPC, no JSON array). */
#[tauri::command]
pub async fn search_download(app: tauri::AppHandle, id: String, url: String, max_mb: Option<u64>, kind: Option<String>) -> Result<tauri::ipc::Response, String> {
    let result = download_inner(&app, &id, &url, max_mb, kind).await;
    clear_cancel(&id);
    result.map(tauri::ipc::Response::new)
}

async fn download_inner(app: &tauri::AppHandle, id: &str, url: &str, max_mb: Option<u64>, kind: Option<String>) -> Result<Vec<u8>, String> {
    let parsed = check_url(url)?;
    let is_image = kind.as_deref() == Some("image");
    let cap: u64 = max_mb.unwrap_or(if is_image { 8 } else { 600 }).clamp(1, 1024) * 1024 * 1024;
    let request = client(if is_image { 30 } else { 3600 })?.get(parsed).send();
    let mut res = watch(id, request).await?.map_err(|e| e.to_string())?;
    let status = res.status();
    if !status.is_success() { return Err(format!("http {}", status.as_u16())); }
    let ctype = res.headers().get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    if ctype.starts_with("text/") || ctype.contains("json") || ctype.contains("xml") {
        return Err(format!("not a media file ({ctype})"));
    }
    if is_image && !ctype.is_empty() && !ctype.starts_with("image/") && ctype != "application/octet-stream" {
        return Err("not an image".into());
    }
    let total = res.content_length().unwrap_or(0);
    if total > cap { return Err("file is too large".into()); }
    let mut buf: Vec<u8> = Vec::with_capacity(total.min(cap).min(64 * 1024 * 1024) as usize);
    let mut last = Instant::now();
    let emit = |received: u64, done: bool| {
        if !is_image { let _ = app.emit_to("main", "nova-search-progress", Progress { id: id.to_string(), received, total, done }); }
    };
    emit(0, false);
    loop {
        let chunk = watch(id, res.chunk()).await?.map_err(|e| e.to_string())?;
        let Some(chunk) = chunk else { break };
        buf.extend_from_slice(&chunk);
        if buf.len() as u64 > cap { return Err("file is too large".into()); }
        if last.elapsed() >= Duration::from_millis(160) { emit(buf.len() as u64, false); last = Instant::now(); }
    }
    if is_cancelled(id) { return Err("cancelled".into()); }
    if buf.is_empty() { return Err("empty file".into()); }
    emit(buf.len() as u64, true);
    Ok(buf)
}

#[tauri::command]
pub fn search_cancel(id: String) {
    if let Ok(mut g) = CANCELLED.lock() {
        let set = g.get_or_insert_with(HashSet::new);
        /* ids are unique per job; keep the set from growing without bound */
        if set.len() > 256 { set.clear(); }
        set.insert(id);
    }
}
