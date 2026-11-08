//! GalleryDesk — offline-first photo and video manager.
//!
//! Module map:
//!   db      schema + migrations           media   EXIF/ffprobe/thumbnail rendering
//!   scan    background indexing           query   catalog reads/writes used by the UI
//!   watch   optional folder watcher       (this file) Tauri state, events and commands

pub mod db;
pub mod exclude;
pub mod media;
pub mod query;
pub mod scan;
pub mod stream;
pub mod watch;

use query::{Filter, Sort};
use rusqlite::Connection;
use scan::{Core, Progress, Request, Sink};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{atomic::{AtomicBool, Ordering}, Arc},
};
use tauri::{AppHandle, Emitter, Manager, State};

type R<T> = Result<T, String>;

struct AppState {
    core: Arc<Core>,
    sink: Arc<dyn Sink>,
    watch: watch::WatchState,
    dupe_cancel: Arc<AtomicBool>,
    video_cancel: std::sync::Mutex<Arc<AtomicBool>>,
    stream: Option<stream::Stream>,
}

struct TauriSink(AppHandle);

impl Sink for TauriSink {
    fn progress(&self, p: &Progress) { let _ = self.0.emit("scan-progress", p); }
    fn changed(&self) { let _ = self.0.emit("library-changed", ()); }
    fn allow_path(&self, path: &Path) {
        // Originals are exposed to the webview read-only, and only for folders the user added.
        let _ = self.0.asset_protocol_scope().allow_directory(path, true);
    }
}

fn conn(s: &State<'_, AppState>) -> R<Connection> { s.core.db.open().map_err(|e| e.to_string()) }

fn data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("GALLERYDESK_DATA_DIR") { return PathBuf::from(p); }
    let base = dirs::data_local_dir().unwrap_or_else(std::env::temp_dir);
    let dir = base.join("io.gallerydesk.app");
    migrate_legacy_dir(&base.join("io.luma.photomanager"), &dir);
    dir
}

/// Carry a catalog over from the pre-rename data folder. The web engine creates the new folder
/// (cache, local storage) before we get here, so "folder doesn't exist" can't be the test: move the
/// old entries over whenever the new folder has no catalog yet. Nothing is overwritten.
pub fn migrate_legacy_dir(old: &Path, new: &Path) {
    if !old.join("library.sqlite3").is_file() || new.join("library.sqlite3").exists() { return; }
    if fs::create_dir_all(new).is_err() { return; }
    let Ok(rd) = fs::read_dir(old) else { return };
    for e in rd.flatten() {
        let dest = new.join(e.file_name());
        if dest.exists() {
            // an empty folder created by us/the web engine (e.g. previews/) is fine to replace
            let empty = dest.is_dir() && fs::read_dir(&dest).map(|mut d| d.next().is_none()).unwrap_or(false);
            if !empty { continue; }
            let _ = fs::remove_dir(&dest);
        }
        let _ = fs::rename(e.path(), &dest);
    }
}

fn known_path(c: &Connection, path: &str) -> R<()> {
    let n: i64 = c.query_row("SELECT COUNT(*) FROM media WHERE path=?1", [path], |r| r.get(0)).map_err(|e| e.to_string())?;
    if n == 0 { Err("That file is not in the library".into()) } else { Ok(()) }
}

fn pct_encode(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) { o.push(b as char) } else { o.push_str(&format!("%{b:02X}")) }
    }
    o
}

fn detach(mut cmd: Command) -> R<()> {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("could not launch: {e}"))?;
    std::thread::spawn(move || { let _ = child.wait(); });
    Ok(())
}

// ------------------------------------------------------------------ commands

#[tauri::command]
async fn app_ready(s: State<'_, AppState>) -> R<()> {
    let c = conn(&s)?;
    let _ = query::requeue_missing_thumbs(&c, &s.core.thumbs_dir);
    let settings = query::get_settings(&c)?;
    s.watch.restart(&s.core, &s.sink, settings["watch"] == "1");
    let scan_launch = settings["scan_on_launch"] == "1";
    scan::request(&s.core, &s.sink, Request { folders: None, discover: scan_launch, retry_failed: false });
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CacheInfo { bytes: u64, files: u64 }
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo { version: String, ffmpeg: bool, ffprobe: bool, data_dir: String, thumbnails: CacheInfo, previews: CacheInfo }

#[tauri::command]
fn video_supported() -> bool { media::gst_playback_ready() }

#[tauri::command]
async fn app_info(s: State<'_, AppState>) -> R<AppInfo> {
    let core = s.core.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (tb, tf) = query::dir_size(&core.thumbs_dir);
        let (pb, pf) = query::dir_size(&core.previews_dir);
        AppInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            ffmpeg: media::tool_available("ffmpeg"),
            ffprobe: media::tool_available("ffprobe"),
            data_dir: core.data_dir.to_string_lossy().into(),
            thumbnails: CacheInfo { bytes: tb, files: tf },
            previews: CacheInfo { bytes: pb, files: pf },
        }
    }).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_settings(s: State<'_, AppState>) -> R<BTreeMap<String, String>> { query::get_settings(&conn(&s)?) }

#[tauri::command]
async fn set_setting(key: String, value: String, s: State<'_, AppState>) -> R<()> {
    let c = conn(&s)?;
    query::set_setting(&c, &key, &value)?;
    if key == "watch" { s.watch.restart(&s.core, &s.sink, value == "1"); }
    Ok(())
}

#[tauri::command]
async fn list_folders(s: State<'_, AppState>) -> R<Vec<query::FolderInfo>> { query::list_folders(&conn(&s)?) }

#[tauri::command]
async fn list_dirs(folder_id: i64, s: State<'_, AppState>) -> R<Vec<query::DirCount>> { query::list_dirs(&conn(&s)?, folder_id) }

#[tauri::command]
async fn add_folder(path: String, s: State<'_, AppState>) -> R<i64> {
    let id = query::add_folder(&conn(&s)?, &path)?;
    s.sink.allow_path(Path::new(&path));
    scan::request(&s.core, &s.sink, Request { folders: Some(HashSet::from([id])), discover: true, retry_failed: false });
    let watching = query::get_settings(&conn(&s)?)?["watch"] == "1";
    s.watch.restart(&s.core, &s.sink, watching);
    Ok(id)
}

#[tauri::command]
async fn remove_folder(id: i64, s: State<'_, AppState>) -> R<()> {
    query::remove_folder(&conn(&s)?, &s.core.thumbs_dir, id)?;
    let watching = query::get_settings(&conn(&s)?)?["watch"] == "1";
    s.watch.restart(&s.core, &s.sink, watching);
    let _ = s.sink.changed();
    Ok(())
}

#[tauri::command]
async fn list_excluded(s: State<'_, AppState>) -> R<Vec<String>> { exclude::list(&conn(&s)?) }

#[tauri::command]
async fn add_excluded(entry: String, s: State<'_, AppState>) -> R<usize> {
    let n = exclude::add(&conn(&s)?, &s.core.thumbs_dir, &entry)?;
    let _ = s.sink.changed();
    Ok(n)
}

#[tauri::command]
async fn remove_excluded(entry: String, s: State<'_, AppState>) -> R<()> {
    exclude::remove(&conn(&s)?, &entry)?;
    // Bring the folder's files back into the library.
    scan::request(&s.core, &s.sink, Request { folders: None, discover: true, retry_failed: false });
    Ok(())
}

#[tauri::command]
async fn start_scan(folder_id: Option<i64>, s: State<'_, AppState>) -> R<()> {
    scan::request(&s.core, &s.sink, Request { folders: folder_id.map(|i| HashSet::from([i])), discover: true, retry_failed: true });
    Ok(())
}

#[tauri::command]
async fn cancel_scan(s: State<'_, AppState>) -> R<()> { scan::cancel(&s.core); Ok(()) }

#[tauri::command]
async fn scan_status(s: State<'_, AppState>) -> R<Progress> { Ok(s.core.progress.lock().unwrap().clone()) }

#[tauri::command]
async fn library_stats(s: State<'_, AppState>) -> R<query::Stats> { query::library_stats(&conn(&s)?) }

#[tauri::command]
async fn query_layout(filter: Filter, group: String, desc: bool, s: State<'_, AppState>) -> R<query::Layout> { query::query_layout(&conn(&s)?, &filter, &group, desc) }

#[tauri::command]
async fn query_media(filter: Filter, sort: Sort, offset: i64, limit: i64, s: State<'_, AppState>) -> R<Vec<query::MediaItem>> {
    query::query_media(&conn(&s)?, &s.core.thumbs_dir, &filter, &sort, offset, limit)
}

#[tauri::command]
async fn query_ids(filter: Filter, sort: Sort, offset: i64, limit: i64, s: State<'_, AppState>) -> R<Vec<i64>> {
    query::query_ids(&conn(&s)?, &filter, &sort, offset, limit)
}

#[tauri::command]
async fn get_media(id: i64, s: State<'_, AppState>) -> R<query::Detail> { query::get_media(&conn(&s)?, &s.core.thumbs_dir, id) }

#[tauri::command]
async fn set_favorite(ids: Vec<i64>, value: bool, s: State<'_, AppState>) -> R<()> { query::set_favorite(&conn(&s)?, &ids, value) }

#[tauri::command]
async fn rotate_media(ids: Vec<i64>, delta: i64, s: State<'_, AppState>) -> R<()> {
    query::rotate(&conn(&s)?, &s.core.thumbs_dir, &ids, delta)?;
    scan::request(&s.core, &s.sink, Request { folders: Some(HashSet::new()), discover: false, retry_failed: false });
    Ok(())
}

#[tauri::command]
async fn list_albums(s: State<'_, AppState>) -> R<Vec<query::NamedCount>> { query::list_albums(&conn(&s)?) }
#[tauri::command]
async fn create_album(name: String, s: State<'_, AppState>) -> R<i64> { query::create_album(&conn(&s)?, &name) }
#[tauri::command]
async fn rename_album(id: i64, name: String, s: State<'_, AppState>) -> R<()> { query::rename_album(&conn(&s)?, id, &name) }
#[tauri::command]
async fn delete_album(id: i64, s: State<'_, AppState>) -> R<()> { query::delete_album(&conn(&s)?, id) }
#[tauri::command]
async fn album_add(album_id: i64, ids: Vec<i64>, s: State<'_, AppState>) -> R<i64> { query::album_add(&conn(&s)?, album_id, &ids) }
#[tauri::command]
async fn album_remove(album_id: i64, ids: Vec<i64>, s: State<'_, AppState>) -> R<()> { query::album_remove(&conn(&s)?, album_id, &ids) }
#[tauri::command]
async fn list_tags(s: State<'_, AppState>) -> R<Vec<query::NamedCount>> { query::list_tags(&conn(&s)?) }
#[tauri::command]
async fn add_tag(ids: Vec<i64>, tag: String, s: State<'_, AppState>) -> R<()> { query::add_tag(&conn(&s)?, &ids, &tag) }
#[tauri::command]
async fn remove_tag(ids: Vec<i64>, tag: String, s: State<'_, AppState>) -> R<()> { query::remove_tag(&conn(&s)?, &ids, &tag) }

/// A large JPEG for the full-screen viewer, used when the original is not browser-displayable
/// (RAW, HEIC, TIFF, ...) or has been rotated. Cached; originals are never touched.
#[tauri::command]
async fn ensure_preview(id: i64, s: State<'_, AppState>) -> R<String> {
    let c = conn(&s)?;
    let (path, bytes, modified, rotation): (String, i64, i64, i64) = c
        .query_row("SELECT path,bytes,modified,rotation FROM media WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map_err(|_| "That item is no longer in the library".to_string())?;
    let core = s.core.clone();
    tauri::async_runtime::spawn_blocking(move || {
        const EDGE: u32 = 2560;
        let out = core.previews_dir.join(format!("{}.jpg", media::cache_key(&path, bytes, modified, rotation, EDGE)));
        if !out.exists() {
            let meta = media::read_exif(Path::new(&path));
            let ok = media::render(
                &media::RenderOpts { src: Path::new(&path), out: &out, long_edge: EDGE, video: false, orientation: meta.orientation, rotation, seek: None, quality: 3 },
                &AtomicBool::new(false),
            );
            if !ok { return Err("Could not decode this image. It may need a codec your FFmpeg build lacks.".to_string()); }
        }
        Ok(out.to_string_lossy().to_string())
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn ensure_video_playback(id: i64, s: State<'_, AppState>) -> R<String> {
    let c = conn(&s)?;
    let (path, bytes, modified): (String, i64, i64) = c
        .query_row("SELECT path,bytes,modified FROM media WHERE id=?1 AND kind='video'", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(|_| "That video is no longer in the library".to_string())?;
    // A newer request (or closing the player) cancels the one still converting.
    let cancel = Arc::new(AtomicBool::new(false));
    { let mut g = s.video_cancel.lock().map_err(|_| "lock".to_string())?; g.store(true, Ordering::SeqCst); *g = cancel.clone(); }
    let core = s.core.clone();
    let stream_ref = s.stream.as_ref().map(|st| stream::Stream { port: st.port, token: st.token.clone() });
    tauri::async_runtime::spawn_blocking(move || {
        let out = core.previews_dir.join(format!("{}.webm", media::cache_key(&path, bytes, modified, 0, 1920)));
        let name = out.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let url = move || stream_ref.as_ref().map(|st| st.preview_url(&name)).ok_or_else(|| "The local media server is not running.".to_string());
        if out.exists() { return url(); }
        if !media::tool_available("ffmpeg") { return Err("FFmpeg is not installed, so this video can't be converted for the built-in player.".to_string()); }
        if media::transcode_for_playback(Path::new(&path), &out, &cancel) { url() }
        else if cancel.load(Ordering::SeqCst) { Err("cancelled".to_string()) }
        else { Err("This video could not be converted for the built-in player.".to_string()) }
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn video_url(id: i64, s: State<'_, AppState>) -> R<String> {
    let c = conn(&s)?;
    let n: i64 = c.query_row("SELECT COUNT(*) FROM media WHERE id=?1 AND kind='video'", [id], |r| r.get(0)).map_err(|e| e.to_string())?;
    if n == 0 { return Err("That video is no longer in the library".into()); }
    s.stream.as_ref().map(|st| st.media_url(id)).ok_or_else(|| "The local media server is not running.".to_string())
}

#[tauri::command]
async fn cancel_video_playback(s: State<'_, AppState>) -> R<()> {
    if let Ok(g) = s.video_cancel.lock() { g.store(true, Ordering::SeqCst); }
    Ok(())
}

#[tauri::command]
async fn reveal_in_folder(path: String, s: State<'_, AppState>) -> R<()> {
    known_path(&conn(&s)?, &path)?;
    let uri = format!("file://{}", pct_encode(&path));
    let selected = media::host_command("dbus-send")
        .args(["--session", "--print-reply", "--reply-timeout=1500", "--dest=org.freedesktop.FileManager1", "--type=method_call", "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1.ShowItems"])
        .arg(format!("array:string:{uri}")).arg("string:")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|st| st.success()).unwrap_or(false);
    if selected { return Ok(()); }
    let parent = Path::new(&path).parent().ok_or("File has no parent folder")?;
    let mut cmd = media::host_command("xdg-open");
    cmd.arg(parent);
    detach(cmd)
}

#[tauri::command]
async fn open_external(path: String, s: State<'_, AppState>) -> R<()> {
    known_path(&conn(&s)?, &path)?;
    let mut cmd = media::host_command("xdg-open");
    cmd.arg(&path);
    detach(cmd)
}

#[tauri::command]
async fn find_duplicates(app: AppHandle, s: State<'_, AppState>) -> R<Vec<query::DupGroup>> {
    s.dupe_cancel.store(false, Ordering::SeqCst);
    let core = s.core.clone();
    let cancel = s.dupe_cancel.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let c = core.db.open().map_err(|e| e.to_string())?;
        query::find_duplicates(&c, &core.thumbs_dir, &cancel, |d, t| { let _ = app.emit("dupes-progress", (d, t)); })
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn cancel_duplicates(s: State<'_, AppState>) -> R<()> { s.dupe_cancel.store(true, Ordering::SeqCst); Ok(()) }

#[tauri::command]
async fn rebuild_thumbnails(s: State<'_, AppState>) -> R<()> {
    let c = conn(&s)?;
    if let Ok(rd) = fs::read_dir(&s.core.thumbs_dir) { for e in rd.flatten() { let _ = fs::remove_file(e.path()); } }
    c.execute("UPDATE media SET thumb=NULL, meta_state=0", []).map_err(|e| e.to_string())?;
    scan::request(&s.core, &s.sink, Request { folders: Some(HashSet::new()), discover: false, retry_failed: false });
    Ok(())
}

#[tauri::command]
async fn clear_preview_cache(s: State<'_, AppState>) -> R<u64> {
    let (bytes, _) = query::dir_size(&s.core.previews_dir);
    if let Ok(rd) = fs::read_dir(&s.core.previews_dir) { for e in rd.flatten() { let _ = fs::remove_file(e.path()); } }
    Ok(bytes)
}

#[tauri::command]
async fn backup_catalog(dest: String, s: State<'_, AppState>) -> R<()> { query::backup_catalog(&conn(&s)?, Path::new(&dest)) }

#[tauri::command]
async fn restore_catalog(src: String, s: State<'_, AppState>) -> R<()> {
    query::validate_backup(Path::new(&src))?;
    fs::copy(&src, s.core.data_dir.join("library.restore.pending")).map_err(|e| format!("could not stage the backup: {e}"))?;
    Ok(())
}

pub fn run() {
    media::fix_gstreamer_env();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let core = Arc::new(Core::new(data_dir()).map_err(|e| {
                eprintln!("gallerydesk: {e}");
                e
            })?);
            let sink: Arc<dyn Sink> = Arc::new(TauriSink(app.handle().clone()));
            sink.allow_path(&core.data_dir);
            if let Ok(c) = core.db.open() {
                if let Ok(folders) = query::list_folders(&c) {
                    for f in folders { sink.allow_path(Path::new(&f.path)); }
                }
            }
            let stream = match stream::start(core.db.clone(), core.previews_dir.clone()) { Ok(st) => Some(st), Err(e) => { eprintln!("gallerydesk: media server unavailable: {e}"); None } };
            app.manage(AppState { core, sink, watch: watch::WatchState::default(), dupe_cancel: Arc::new(AtomicBool::new(false)), video_cancel: std::sync::Mutex::new(Arc::new(AtomicBool::new(false))), stream });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_ready, app_info, video_supported, get_settings, set_setting, list_folders, list_dirs, add_folder, remove_folder,
            list_excluded, add_excluded, remove_excluded, ensure_video_playback, cancel_video_playback, video_url, start_scan, cancel_scan, scan_status, library_stats, query_layout, query_media, query_ids, get_media,
            set_favorite, rotate_media, list_albums, create_album, rename_album, delete_album, album_add,
            album_remove, list_tags, add_tag, remove_tag, ensure_preview, reveal_in_folder, open_external,
            find_duplicates, cancel_duplicates, rebuild_thumbnails, clear_preview_cache, backup_catalog, restore_catalog
        ])
        .run(tauri::generate_context!())
        .expect("error while running GalleryDesk");
}
