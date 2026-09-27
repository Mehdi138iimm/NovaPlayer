use serde::Serialize;
use std::{collections::HashSet, fs, path::{Path, PathBuf}, sync::{Arc, atomic::{AtomicBool, Ordering}}};
use tauri::{Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

mod update;

#[derive(Default)]
struct AppState {
    close_to_tray: AtomicBool,
    storage_directory: std::sync::Mutex<Option<String>>,
    referenced_audio: std::sync::Mutex<HashSet<PathBuf>>,
    main_hidden: AtomicBool,
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
fn collect_audio(path: &Path, out: &mut Vec<PathBuf>) {
    fn visit(path: &Path, out: &mut Vec<PathBuf>, depth: usize) {
        if depth > 64 || out.len() > 500 { return; }
        let metadata = match fs::symlink_metadata(path) { Ok(value) => value, Err(_) => return };
        if metadata.file_type().is_symlink() { return; }
        if metadata.is_file() {
            if audio_mime(path).is_some() { out.push(path.to_path_buf()); }
            return;
        }
        if !metadata.is_dir() { return; }
        if let Ok(items) = fs::read_dir(path) {
            for item in items.flatten() {
                if out.len() > 500 { break; }
                visit(&item.path(), out, depth + 1);
            }
        }
    }
    visit(path, out, 0);
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
    if let Some(w)=app.get_webview_window("mini") { w.show().map_err(|e|e.to_string())?; w.set_focus().map_err(|e|e.to_string())?; return Ok(()); }
    WebviewWindowBuilder::new(&app,"mini",WebviewUrl::App("mini.html".into())).title("NOVA Mini").center().inner_size(340.0,540.0).min_inner_size(300.0,470.0).always_on_top(true).decorations(true).resizable(true).build().map_err(|e|e.to_string())?;
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
    let state=Arc::new(AppState{close_to_tray:AtomicBool::new(true), storage_directory: std::sync::Mutex::new(None), referenced_audio: std::sync::Mutex::new(HashSet::new()), main_hidden: AtomicBool::new(false)});
    tauri::Builder::default()
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
        tray.on_menu_event(|app,event|match event.id().as_ref(){"show"=>{if let Some(w)=app.get_webview_window("main"){let _=w.show();let _=w.unminimize();let _=w.set_focus();if let Some(st)=app.try_state::<Arc<AppState>>(){set_main_hidden(app,&st,false);}}},"quit"=>app.exit(0),_=>{}}).build(app)?;
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
      .invoke_handler(tauri::generate_handler![update::check_github_update,fetch_trend_preview,register_audio_paths,read_dropped_audio,read_audio_file,delete_audio_file,show_mini_player,close_mini_player,set_close_to_tray,start_main_window_drag,control_main_window,set_storage_directory,get_storage_directory,save_audio_file])
      .run(tauri::generate_context!()).expect("NOVA failed to start");
}
