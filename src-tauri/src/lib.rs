use serde::Serialize;
use std::{collections::HashSet, fs, path::{Path, PathBuf}, sync::{Arc, atomic::{AtomicBool, Ordering}}};
use tauri::{Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

mod update;
mod search;

#[derive(Default)]
struct AppState {
    close_to_tray: AtomicBool,
    storage_directory: std::sync::Mutex<Option<String>>,
    referenced_audio: std::sync::Mutex<HashSet<PathBuf>>,
    main_hidden: AtomicBool,
    /* v1.10.0: audio files passed on the command line (Explorer double-click / Open with) */
    launch_files: std::sync::Mutex<Vec<String>>,
}

/* Perf: tell the page when the main window is hidden (tray) or minimized so every
   animation loop and the visualizer stop drawing frames nobody can see. */
fn set_main_hidden(app: &tauri::AppHandle, state: &AppState, hidden: bool) {
    if state.main_hidden.swap(hidden, Ordering::Relaxed) != hidden {
        let _ = app.emit_to("main", "nova-window-visibility", hidden);
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(v) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v); i += 3; continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Serialize)]
struct AudioFile { name: String, mime: String, data: Vec<u8> }

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioReference {
    name: String,
    mime: String,
    path: String,
    size: u64,
    folder: String,
}

fn audio_root(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?.join("audio");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

#[tauri::command]
fn set_storage_directory(state: State<'_, Arc<AppState>>, path: String) -> Result<(), String> {
    if path.trim().is_empty() { return Err("مسیر ذخیره‌سازی خالی است".into()); }
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    state.storage_directory.lock().map_err(|_| "خطای قفل مسیر".to_string())?.replace(path);
    Ok(())
}

#[tauri::command]
fn get_storage_directory(state: State<'_, Arc<AppState>>) -> Option<String> {
    state.storage_directory.lock().ok()?.clone()
}

/* Perf: the page sends raw bytes (binary IPC) instead of a JSON array of numbers,
   which used to cost ~10x the file size in RAM and a lot of CPU to (de)serialize. */
#[tauri::command]
fn save_audio_file(app: tauri::AppHandle, state: State<'_, Arc<AppState>>, request: tauri::ipc::Request<'_>) -> Result<Option<String>, String> {
    let (name, data): (String, Vec<u8>) = match request.body() {
        tauri::ipc::InvokeBody::Raw(bytes) => {
            let name = request.headers().get("x-nova-name")
                .and_then(|v| v.to_str().ok()).map(percent_decode)
                .ok_or("نام فایل نامعتبر")?;
            (name, bytes.clone())
        }
        tauri::ipc::InvokeBody::Json(value) => {
            let name = value.get("name").and_then(|v| v.as_str()).ok_or("نام فایل نامعتبر")?.to_string();
            let data: Vec<u8> = serde_json::from_value(value.get("data").cloned().unwrap_or_default()).map_err(|e| e.to_string())?;
            (name, data)
        }
    };
    let dir = state.storage_directory.lock().map_err(|_| "خطای قفل مسیر".to_string())?.clone()
        .map(PathBuf::from).unwrap_or(audio_root(&app)?);
    let safe_name = Path::new(&name).file_name().ok_or("نام فایل نامعتبر")?.to_string_lossy().to_string();
    let mut path = dir.join(&safe_name);
    if path.exists() {
        let stem = Path::new(&safe_name).file_stem().unwrap_or_default().to_string_lossy();
        let ext = Path::new(&safe_name).extension().map(|x| format!(".{}",x.to_string_lossy())).unwrap_or_default();
        let mut n = 2;
        while path.exists() { path = dir.join(format!("{} ({}){}", stem, n, ext)); n += 1; }
    }
    fs::write(&path, data).map_err(|e| e.to_string())?;
    Ok(Some(path.to_string_lossy().to_string()))
}

fn allowed_audio_path(app: &tauri::AppHandle, state: &State<'_, Arc<AppState>>, path: &Path) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(path).map_err(|error| error.to_string())?;
    let app_root = fs::canonicalize(audio_root(app)?).map_err(|error| error.to_string())?;
    let custom_root = state.storage_directory.lock()
        .map_err(|_| "خطای قفل مسیر".to_string())?
        .clone()
        .map(PathBuf::from)
        .and_then(|root| fs::canonicalize(root).ok());
    let allowed = canonical.starts_with(&app_root)
        || custom_root.as_ref().is_some_and(|root| canonical.starts_with(root))
        || state.referenced_audio.lock()
            .map_err(|_| "خطای قفل فایل‌های مرجع".to_string())?
            .contains(&canonical);
    if !allowed { return Err("مسیر فایل صوتی مجاز نیست".into()); }
    Ok(canonical)
}

#[tauri::command]
fn register_audio_paths(state: State<'_, Arc<AppState>>, paths: Vec<String>) -> Result<Vec<AudioReference>, String> {
    let mut found = Vec::new();
    for path in paths { collect_audio(Path::new(&path), &mut found); }
    if found.len() > 500 { return Err("حداکثر ۵۰۰ فایل را یک‌جا وارد کن".into()); }
    let mut allowed = state.referenced_audio.lock().map_err(|_| "خطای قفل فایل‌های مرجع".to_string())?;
    let mut result = Vec::new();
    for path in found {
        let canonical = fs::canonicalize(&path).map_err(|error| error.to_string())?;
        let metadata = fs::metadata(&canonical).map_err(|error| error.to_string())?;
        let mime = audio_mime(&canonical).ok_or("فرمت صوتی پشتیبانی نمی‌شود")?.to_string();
        allowed.insert(canonical.clone());
        result.push(AudioReference {
            name: canonical.file_name().unwrap_or_default().to_string_lossy().to_string(),
            mime,
            path: canonical.to_string_lossy().to_string(),
            size: metadata.len(),
            folder: canonical.parent().and_then(Path::file_name).unwrap_or_default().to_string_lossy().to_string(),
        });
    }
    Ok(result)
}

/* Perf: returned as a binary ArrayBuffer (not a JSON number array) and read off the
   UI thread, so switching tracks no longer spikes CPU/RAM or freezes the window. */
#[tauri::command]
async fn read_audio_file(app: tauri::AppHandle, state: State<'_, Arc<AppState>>, path: String) -> Result<tauri::ipc::Response, String> {
    let canonical = allowed_audio_path(&app, &state, Path::new(&path))?;
    let data = fs::read(canonical).map_err(|e| e.to_string())?;
    Ok(tauri::ipc::Response::new(data))
}

/* v1.7.1: TRENDING previews are downloaded here (no CORS in Rust) and handed to the
   page as bytes. Played from a same-origin blob: URL, the Web Audio visualizer graph
   no longer mutes them. Only Apple's preview CDNs are allowed. */
#[tauri::command]
async fn fetch_trend_preview(url: String) -> Result<tauri::ipc::Response, String> {
    let parsed = reqwest::Url::parse(&url).map_err(|e| e.to_string())?;
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let allowed = parsed.scheme() == "https"
        && ["apple.com", "mzstatic.com"].iter().any(|d| host == *d || host.ends_with(&format!(".{d}")));
    if !allowed { return Err("preview host not allowed".into()); }
    let client = reqwest::Client::builder()
        .user_agent("NOVA-Player")
        .timeout(std::time::Duration::from_secs(25))
        .connect_timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;
    let res = client.get(parsed).send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() { return Err(format!("preview http {}", res.status())); }
    let bytes = res.bytes().await.map_err(|e| e.to_string())?;
    if bytes.is_empty() || bytes.len() > 20 * 1024 * 1024 { return Err("preview size invalid".into()); }
    Ok(tauri::ipc::Response::new(bytes.to_vec()))
}

/* v1.8.1: online synced lyrics from LRCLIB (free, no API key). Done in Rust so CORS and
   WebView quirks never matter; only lrclib.net /api/get and /api/search are reachable. */
#[tauri::command]
async fn fetch_lrclib(endpoint: String, params: Vec<(String, String)>) -> Result<String, String> {
    let path = match endpoint.as_str() { "get" => "api/get", "search" => "api/search", _ => return Err("lrclib endpoint not allowed".into()) };
    let allowed = ["track_name", "artist_name", "album_name", "duration", "q"];
    let clean: Vec<(String, String)> = params.into_iter()
        .filter(|(k, v)| allowed.contains(&k.as_str()) && !v.trim().is_empty() && v.len() <= 300)
        .collect();
    if clean.is_empty() { return Err("empty lyrics query".into()); }
    let mut url = reqwest::Url::parse("https://lrclib.net/").map_err(|e| e.to_string())?.join(path).map_err(|e| e.to_string())?;
    url.query_pairs_mut().extend_pairs(clean.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let client = reqwest::Client::builder()
        .user_agent(concat!("NOVA-Player/", env!("CARGO_PKG_VERSION"), " (https://github.com/Mehdi138iimm/NovaPlayer)"))
        .timeout(std::time::Duration::from_secs(12))
        .connect_timeout(std::time::Duration::from_secs(6))
        .build()
        .map_err(|e| e.to_string())?;
    let res = client.get(url).send().await.map_err(|e| e.to_string())?;
    let status = res.status();
    if status == reqwest::StatusCode::NOT_FOUND { return Ok("null".into()); }
    if !status.is_success() { return Err(format!("lrclib http {}", status)); }
    let body = res.text().await.map_err(|e| e.to_string())?;
    if body.len() > 4 * 1024 * 1024 { return Err("lrclib response too large".into()); }
    Ok(body)
}

/* v1.9.0: second lyrics source (plain text only) when LRCLIB has nothing. Only
   api.lyrics.ovh/v1/{artist}/{title} is reachable. */
#[tauri::command]
async fn fetch_lyrics_ovh(artist: String, title: String) -> Result<String, String> {
    let (artist, title) = (artist.trim().to_string(), title.trim().to_string());
    if artist.is_empty() || title.is_empty() || artist.len() > 200 || title.len() > 200 { return Err("lyrics query invalid".into()); }
    let mut url = reqwest::Url::parse("https://api.lyrics.ovh/v1/").map_err(|e| e.to_string())?;
    url.path_segments_mut().map_err(|_| "lyrics url invalid".to_string())?.pop_if_empty().push(&artist).push(&title);
    let client = reqwest::Client::builder()
        .user_agent(concat!("NOVA-Player/", env!("CARGO_PKG_VERSION"), " (https://github.com/Mehdi138iimm/NovaPlayer)"))
        .timeout(std::time::Duration::from_secs(10))
        .connect_timeout(std::time::Duration::from_secs(6))
        .build()
        .map_err(|e| e.to_string())?;
    let res = client.get(url).send().await.map_err(|e| e.to_string())?;
    let status = res.status();
    if status == reqwest::StatusCode::NOT_FOUND { return Ok("null".into()); }
    if !status.is_success() { return Err(format!("lyrics.ovh http {}", status)); }
    let body = res.text().await.map_err(|e| e.to_string())?;
    if body.len() > 1024 * 1024 { return Err("lyrics response too large".into()); }
    Ok(body)
}

/* v1.9.0: lyric translation (default: Persian). Google's free "gtx" endpoint first,
   MyMemory as a fallback. Only these two hosts are reachable; text is capped per call. */
#[tauri::command]
async fn fetch_translation(provider: String, text: String, target: String, source: Option<String>) -> Result<String, String> {
    if text.trim().is_empty() || text.len() > 6000 { return Err("translation text size invalid".into()); }
    let lang = |v: &str| -> String { v.chars().filter(|c| c.is_ascii_alphabetic() || *c == '-').take(10).collect() };
    let target = lang(&target);
    if target.is_empty() { return Err("translation target invalid".into()); }
    let source = source.map(|v| lang(&v)).filter(|v| !v.is_empty() && v != "auto");
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) NOVA-Player")
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(6))
        .build()
        .map_err(|e| e.to_string())?;
    let req = match provider.as_str() {
        "google" => {
            let sl = source.clone().unwrap_or_else(|| "auto".into());
            client.get("https://translate.googleapis.com/translate_a/single")
                .query(&[("client", "gtx"), ("sl", sl.as_str()), ("tl", target.as_str()), ("dt", "t"), ("ie", "UTF-8"), ("oe", "UTF-8"), ("q", text.as_str())])
        }
        "mymemory" => {
            if text.len() > 500 { return Err("mymemory text too long".into()); }
            let pair = format!("{}|{}", source.unwrap_or_else(|| "en".into()), target);
            client.get("https://api.mymemory.translated.net/get").query(&[("q", text.as_str()), ("langpair", pair.as_str())])
        }
        _ => return Err("translation provider not allowed".into()),
    };
    let res = req.send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() { return Err(format!("translate http {}", res.status())); }
    let body = res.text().await.map_err(|e| e.to_string())?;
    if body.len() > 2 * 1024 * 1024 { return Err("translation response too large".into()); }
    Ok(body)
}

#[tauri::command]
fn delete_audio_file(app: tauri::AppHandle, state: State<'_, Arc<AppState>>, path: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    if !path.exists() { return Ok(()); }
    let canonical = allowed_audio_path(&app, &state, &path)?;
    fs::remove_file(canonical).map_err(|error| error.to_string())
}

fn audio_mime(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_string_lossy().to_ascii_lowercase().as_str() {
        "mp3" => Some("audio/mpeg"), "m4a" | "mp4" => Some("audio/mp4"),
        "ogg" => Some("audio/ogg"), "opus" => Some("audio/opus"),
        "wav" => Some("audio/wav"), "flac" => Some("audio/flac"), _ => None
    }
}
fn collect_audio(path: &Path, out: &mut Vec<PathBuf>) { collect_audio_limit(path, out, 500); }
fn collect_audio_limit(path: &Path, out: &mut Vec<PathBuf>, limit: usize) {
    fn visit(path: &Path, out: &mut Vec<PathBuf>, depth: usize, limit: usize) {
        if depth > 64 || out.len() > limit { return; }
        let metadata = match fs::symlink_metadata(path) { Ok(value) => value, Err(_) => return };
        if metadata.file_type().is_symlink() { return; }
        if metadata.is_file() {
            if audio_mime(path).is_some() { out.push(path.to_path_buf()); }
            return;
        }
        if !metadata.is_dir() { return; }
        if let Ok(items) = fs::read_dir(path) {
            for item in items.flatten() {
                if out.len() > limit { break; }
                visit(&item.path(), out, depth + 1, limit);
            }
        }
    }
    visit(path, out, 0, limit);
}

/* ── v1.10.0 ─────────────────────────────────────────────────────────────── */

fn audio_reference(canonical: &Path, size: u64, mime: String) -> AudioReference {
    AudioReference {
        name: canonical.file_name().unwrap_or_default().to_string_lossy().to_string(),
        mime,
        path: canonical.to_string_lossy().to_string(),
        size,
        folder: canonical.parent().and_then(Path::file_name).unwrap_or_default().to_string_lossy().to_string(),
    }
}

fn inspect_audio(path: &Path) -> Option<(PathBuf, u64, String)> {
    let canonical = fs::canonicalize(path).ok()?;
    let metadata = fs::metadata(&canonical).ok()?;
    if !metadata.is_file() { return None; }
    let mime = audio_mime(&canonical)?.to_string();
    Some((canonical, metadata.len(), mime))
}

/* Command-line arguments that are real audio files (first launch and second instance). */
fn launch_audio_args<I: IntoIterator<Item = String>>(args: I, cwd: Option<&Path>) -> Vec<String> {
    let mut out = Vec::new();
    for raw in args {
        let raw = raw.trim().trim_matches('"').to_string();
        if raw.is_empty() || raw.starts_with('-') { continue; }
        let mut path = PathBuf::from(&raw);
        if path.is_relative() { if let Some(base) = cwd { path = base.join(path); } }
        if path.is_file() && audio_mime(&path).is_some() { out.push(path.to_string_lossy().to_string()); }
        if out.len() >= 500 { break; }
    }
    out
}

#[tauri::command]
fn take_launch_files(state: State<'_, Arc<AppState>>) -> Vec<String> {
    match state.launch_files.lock() { Ok(mut files) => std::mem::take(&mut *files), Err(_) => Vec::new() }
}

/* M3U import and backup restore: check many paths at once; missing ones come back as null
   instead of failing the whole call. Found files are registered for playback. */
#[tauri::command]
fn probe_audio_paths(state: State<'_, Arc<AppState>>, paths: Vec<String>) -> Result<Vec<Option<AudioReference>>, String> {
    if paths.len() > 20000 { return Err("too many paths".into()); }
    let mut allowed = state.referenced_audio.lock().map_err(|_| "خطای قفل فایل‌های مرجع".to_string())?;
    let mut out = Vec::with_capacity(paths.len());
    for raw in paths {
        let raw = raw.trim().to_string();
        if raw.is_empty() { out.push(None); continue; }
        match inspect_audio(Path::new(&raw)) {
            Some((canonical, size, mime)) => { allowed.insert(canonical.clone()); out.push(Some(audio_reference(&canonical, size, mime))); }
            None => out.push(None),
        }
    }
    Ok(out)
}

/* Backup restore: find moved songs by file name inside a folder the user picked. */
#[tauri::command]
async fn scan_audio_folder(state: State<'_, Arc<AppState>>, path: String) -> Result<Vec<AudioReference>, String> {
    let root = PathBuf::from(path.trim());
    if !root.is_dir() { return Err("پوشه پیدا نشد".into()); }
    let found = tauri::async_runtime::spawn_blocking(move || { let mut v = Vec::new(); collect_audio_limit(&root, &mut v, 20000); v })
        .await.map_err(|e| e.to_string())?;
    let mut allowed = state.referenced_audio.lock().map_err(|_| "خطای قفل فایل‌های مرجع".to_string())?;
    let mut out = Vec::with_capacity(found.len());
    for p in found {
        if let Some((canonical, size, mime)) = inspect_audio(&p) {
            allowed.insert(canonical.clone());
            out.push(audio_reference(&canonical, size, mime));
        }
    }
    Ok(out)
}

fn text_file_allowed(path: &Path) -> bool {
    matches!(path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).as_deref(), Some("m3u") | Some("m3u8") | Some("json"))
}

/* Only playlist (.m3u/.m3u8) and backup (.json) files, picked by the user in a dialog. */
#[tauri::command]
fn read_text_file(path: String) -> Result<String, String> {
    let path = PathBuf::from(path);
    if !text_file_allowed(&path) { return Err("نوع فایل مجاز نیست".into()); }
    let meta = fs::metadata(&path).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() > 64 * 1024 * 1024 { return Err("حجم فایل نامعتبر است".into()); }
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    let bytes = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) { &bytes[3..] } else { &bytes[..] };
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

#[tauri::command]
fn write_text_file(path: String, contents: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    if !text_file_allowed(&path) { return Err("نوع فایل مجاز نیست".into()); }
    if contents.len() > 128 * 1024 * 1024 { return Err("فایل خیلی بزرگ است".into()); }
    if let Some(parent) = path.parent() { if !parent.as_os_str().is_empty() && !parent.is_dir() { return Err("پوشهٔ مقصد پیدا نشد".into()); } }
    fs::write(&path, contents).map_err(|e| e.to_string())
}

/* Real reachability check (navigator.onLine only knows about the network adapter). */
#[tauri::command]
async fn net_probe() -> bool {
    tauri::async_runtime::spawn_blocking(|| {
        use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
        let timeout = std::time::Duration::from_millis(2500);
        for host in ["lrclib.net:443", "api.github.com:443"] {
            if let Ok(addrs) = host.to_socket_addrs() {
                for addr in addrs.take(2) { if TcpStream::connect_timeout(&addr, timeout).is_ok() { return true; } }
            }
        }
        for ip in ["1.1.1.1:443", "8.8.8.8:443", "9.9.9.9:443"] {
            if let Ok(addr) = ip.parse::<SocketAddr>() { if TcpStream::connect_timeout(&addr, timeout).is_ok() { return true; } }
        }
        false
    }).await.unwrap_or(false)
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") { let _ = w.show(); let _ = w.unminimize(); let _ = w.set_focus(); }
    if let Some(st) = app.try_state::<Arc<AppState>>() { set_main_hidden(app, &st, false); }
}

#[tauri::command]
async fn read_dropped_audio(paths: Vec<String>) -> Result<Vec<AudioFile>, String> {
    let mut found=Vec::new(); for path in paths { collect_audio(Path::new(&path), &mut found); }
    if found.len()>500 { return Err("حداکثر ۵۰۰ فایل را یک‌جا وارد کن".into()); }
    let mut result=Vec::new();
    for path in found { let mime=audio_mime(&path).unwrap_or("application/octet-stream").to_string(); let data=fs::read(&path).map_err(|e|e.to_string())?; let name=path.file_name().unwrap_or_default().to_string_lossy().to_string(); result.push(AudioFile{name,mime,data}); }
    Ok(result)
}

/* Must be async: building a window inside a sync command deadlocks WebView2 on
   Windows (white window + frozen app). Async commands run off the main thread. */
#[tauri::command]
async fn show_mini_player(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w)=app.get_webview_window("mini") { let _=w.unminimize(); w.show().map_err(|e|e.to_string())?; w.set_focus().map_err(|e|e.to_string())?; return Ok(()); }
    /* v1.8.1: mini.html draws its own title bar (drag, pin, minimize, close), so the native
       frame is off. With decorations on, Windows showed two title bars and a maximize button. */
    WebviewWindowBuilder::new(&app,"mini",WebviewUrl::App("mini.html".into())).title("NOVA Mini").center().inner_size(340.0,540.0).min_inner_size(300.0,470.0).always_on_top(true).decorations(false).shadow(true).maximizable(false).resizable(true).focused(true).build().map_err(|e|e.to_string())?;
    Ok(())
}
#[tauri::command]
async fn close_mini_player(app: tauri::AppHandle) -> Result<(), String> { if let Some(w)=app.get_webview_window("mini") { w.destroy().map_err(|e|e.to_string())?; } let _=app.emit("nova-mini-closed",()); Ok(()) }
#[tauri::command]
fn set_close_to_tray(state: State<'_,Arc<AppState>>, enabled: bool) { state.close_to_tray.store(enabled,Ordering::Relaxed); }
#[tauri::command]
fn start_main_window_drag(app: tauri::AppHandle) -> Result<(),String> { app.get_webview_window("main").ok_or("پنجره پیدا نشد")?.start_dragging().map_err(|e|e.to_string()) }
#[tauri::command]
fn control_main_window(app: tauri::AppHandle,state:State<'_,Arc<AppState>>,action:String)->Result<bool,String>{
    let w=app.get_webview_window("main").ok_or("پنجره پیدا نشد")?;
    match action.as_str(){"minimize"=>{w.minimize().map_err(|e|e.to_string())?;set_main_hidden(&app,&state,true);},"toggle_maximize"=>{if w.is_maximized().map_err(|e|e.to_string())?{w.unmaximize()}else{w.maximize()}.map_err(|e|e.to_string())?},"close"=>{if state.close_to_tray.load(Ordering::Relaxed){w.hide().map_err(|e|e.to_string())?;set_main_hidden(&app,&state,true);}else{w.close().map_err(|e|e.to_string())?}},"state"=>{},_=>return Err("فرمان نامعتبر".into())};
    w.is_maximized().map_err(|e|e.to_string())
}

pub fn run(){
    let launch = launch_audio_args(std::env::args().skip(1), std::env::current_dir().ok().as_deref());
    let state=Arc::new(AppState{close_to_tray:AtomicBool::new(true), storage_directory: std::sync::Mutex::new(None), referenced_audio: std::sync::Mutex::new(HashSet::new()), main_hidden: AtomicBool::new(false), launch_files: std::sync::Mutex::new(launch)});
    tauri::Builder::default()
      /* v1.10.0: one NOVA at a time. A second launch (e.g. double-clicking an mp3 while NOVA
         runs) hands its files to this window and exits. Must be the first plugin. */
      .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
        let files = launch_audio_args(argv.into_iter().skip(1), Some(Path::new(&cwd)));
        show_main_window(app);
        if !files.is_empty() { let _ = app.emit_to("main", "nova-open-files", files); }
      }))
      .plugin(tauri_plugin_dialog::init())
      .plugin(tauri_plugin_opener::init())
      .manage(state.clone())
      .setup(|app|{
        use tauri::{menu::{Menu,MenuItem},tray::TrayIconBuilder};
        let show=MenuItem::with_id(app,"show","نمایش NOVA",true,None::<&str>)?;
        let quit=MenuItem::with_id(app,"quit","خروج",true,None::<&str>)?;
        let menu=Menu::with_items(app,&[&show,&quit])?;
        let mut tray=TrayIconBuilder::new().menu(&menu).tooltip("NOVA Player");
        if let Some(icon)=app.default_window_icon(){tray=tray.icon(icon.clone());}
        tray.on_menu_event(|app,event|match event.id().as_ref(){"show"=>show_main_window(app),"quit"=>app.exit(0),_=>{}}).build(app)?;
        Ok(())
      })
      .on_window_event(move|window,event|{
        if window.label()=="mini" {
          /* Perf: really close the mini window (it is rebuilt on demand) instead of
             keeping a hidden renderer alive in the background. */
          if let tauri::WindowEvent::CloseRequested{..}=event {
            let _=window.app_handle().emit("nova-mini-closed",());
          }
        }
        if window.label()=="main" {
          match event {
            tauri::WindowEvent::CloseRequested{api,..} => { if state.close_to_tray.load(Ordering::Relaxed){api.prevent_close();let _=window.hide();set_main_hidden(window.app_handle(),&state,true);} }
            tauri::WindowEvent::Resized(_) | tauri::WindowEvent::Focused(true) => {
              let hidden=window.is_minimized().unwrap_or(false)||!window.is_visible().unwrap_or(true);
              set_main_hidden(window.app_handle(),&state,hidden);
            }
            _ => {}
          }
        }
      })
      .invoke_handler(tauri::generate_handler![update::check_github_update,search::search_http_get,search::search_download,search::search_cancel,fetch_trend_preview,fetch_lrclib,fetch_lyrics_ovh,fetch_translation,register_audio_paths,read_dropped_audio,read_audio_file,delete_audio_file,show_mini_player,close_mini_player,set_close_to_tray,start_main_window_drag,control_main_window,set_storage_directory,get_storage_directory,save_audio_file,take_launch_files,probe_audio_paths,scan_audio_folder,read_text_file,write_text_file,net_probe])
      .run(tauri::generate_context!()).expect("NOVA failed to start");
}
