//! Background indexing: discover files, reconcile deletions/moves, then extract metadata and
//! render thumbnails on a small worker pool. Runs entirely off the UI thread and the UI is
//! told about it through a `Sink` (Tauri events in the app, a recorder in tests).

use crate::db::{now_secs, setting, Db};
use crate::media::{self, cache_key, media_kind, Meta, RenderOpts};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};
use walkdir::WalkDir;

#[derive(Serialize, Clone, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub running: bool,
    pub cancelled: bool,
    /// "idle" | "discover" | "process"
    pub phase: String,
    pub folder: String,
    pub discovered: u64,
    pub processed: u64,
    pub total: u64,
    pub failed: u64,
}

pub trait Sink: Send + Sync {
    fn progress(&self, p: &Progress);
    fn changed(&self);
    fn allow_path(&self, _path: &Path) {}
}

#[derive(Clone, Default)]
pub struct Request {
    /// None = every folder.
    pub folders: Option<HashSet<i64>>,
    pub discover: bool,
    pub retry_failed: bool,
}

impl Request {
    fn merge(&mut self, other: Request) {
        self.folders = match (self.folders.take(), other.folders) {
            (Some(mut a), Some(b)) => { a.extend(b); Some(a) }
            _ => None,
        };
        self.discover |= other.discover;
        self.retry_failed |= other.retry_failed;
    }
}

pub struct Core {
    pub db: Db,
    pub data_dir: PathBuf,
    pub thumbs_dir: PathBuf,
    pub previews_dir: PathBuf,
    pub running: AtomicBool,
    pub cancel: AtomicBool,
    pub pending: Mutex<Option<Request>>,
    pub progress: Mutex<Progress>,
}

impl Core {
    pub fn new(data_dir: PathBuf) -> Result<Core, String> {
        let thumbs_dir = data_dir.join("thumbnails");
        let previews_dir = data_dir.join("previews");
        fs::create_dir_all(&thumbs_dir).map_err(|e| format!("cannot create cache dir: {e}"))?;
        fs::create_dir_all(&previews_dir).map_err(|e| format!("cannot create cache dir: {e}"))?;
        let db_path = data_dir.join("library.sqlite3");
        crate::db::apply_pending_restore(&data_dir, &db_path);
        let db = Db { path: db_path };
        crate::db::init(&db, &thumbs_dir)?;
        // After an upgrade, give previews that failed under an older build another chance and make sure
        // the built-in video player is on again (an old crash guard may have switched it off for good).
        if let Ok(c) = db.open() {
            let cur = env!("CARGO_PKG_VERSION");
            if crate::db::setting(&c, "app_version").as_deref() != Some(cur) {
                let _ = c.execute("UPDATE media SET meta_state=0 WHERE meta_state=2", []);
                for (k, v) in [("video_builtin", "1"), ("video_guard", "0"), ("app_version", cur)] {
                    let _ = c.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![k, v]);
                }
            }
        }
        Ok(Core {
            db,
            data_dir,
            thumbs_dir,
            previews_dir,
            running: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            pending: Mutex::new(None),
            progress: Mutex::new(Progress { phase: "idle".into(), ..Default::default() }),
        })
    }
}

/// Queue a job. If one is already running the request is merged and runs right after it.
pub fn request(core: &Arc<Core>, sink: &Arc<dyn Sink>, req: Request) {
    {
        let mut p = core.pending.lock().unwrap();
        match p.as_mut() {
            Some(existing) => existing.merge(req),
            None => *p = Some(req),
        }
    }
    if core.running.swap(true, Ordering::SeqCst) {
        return;
    }
    core.cancel.store(false, Ordering::SeqCst);
    let core = core.clone();
    let sink = sink.clone();
    thread::spawn(move || loop {
        let next = {
            let mut p = core.pending.lock().unwrap();
            match p.take() {
                Some(r) => r,
                None => {
                    core.running.store(false, Ordering::SeqCst);
                    break;
                }
            }
        };
        if core.cancel.load(Ordering::SeqCst) {
            core.cancel.store(false, Ordering::SeqCst);
        }
        run_job(&core, sink.as_ref(), next);
    });
    // `running` is cleared above under the pending lock, so a request that arrives while the
    // worker is finishing is either picked up by it or starts a fresh thread.
}

pub fn cancel(core: &Core) {
    core.cancel.store(true, Ordering::SeqCst);
    *core.pending.lock().unwrap() = None;
}

fn publish(core: &Core, sink: &dyn Sink, f: impl FnOnce(&mut Progress)) {
    let snapshot = {
        let mut p = core.progress.lock().unwrap();
        f(&mut p);
        p.clone()
    };
    sink.progress(&snapshot);
}

fn is_hidden(e: &walkdir::DirEntry) -> bool {
    if e.depth() == 0 { return false; }
    let Some(n) = e.file_name().to_str() else { return false };
    if n.starts_with('.') { return true; }
    // Developer / package-manager folders hold thousands of icons and screenshots that are not "photos"
    // and make a scan of a home folder crawl.
    e.file_type().is_dir() && matches!(n, "node_modules" | "__pycache__" | "site-packages" | "venv" | "snap" | "target" | "Trash" | "lost+found")
}

fn remove_thumb(core: &Core, name: &Option<String>) {
    if let Some(n) = name {
        let _ = fs::remove_file(core.thumbs_dir.join(n));
    }
}

pub fn run_job(core: &Core, sink: &dyn Sink, req: Request) {
    publish(core, sink, |p| {
        *p = Progress { running: true, phase: "discover".into(), ..Default::default() };
    });

    let folders: Vec<(i64, String)> = match core.db.open().and_then(|c| {
        let mut s = c.prepare("SELECT id,path FROM folders ORDER BY id")?;
        let rows = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>()
    }) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("gallerydesk: scan could not read folders: {e}");
            Vec::new()
        }
    };
    let folders: Vec<_> = folders
        .into_iter()
        .filter(|(id, _)| req.folders.as_ref().map(|s| s.contains(id)).unwrap_or(true))
        .collect();

    if req.retry_failed {
        if let Ok(c) = core.db.open() {
            let _ = c.execute("UPDATE media SET meta_state=0 WHERE meta_state=2", []);
        }
    }

    if req.discover {
        // Previews are rendered *while* the folders are still being walked, so thumbnails start
        // appearing within seconds even when a folder (e.g. a whole home directory) takes minutes.
        let walking = std::sync::atomic::AtomicBool::new(true);
        thread::scope(|scope| {
            scope.spawn(|| {
                while walking.load(Ordering::Relaxed) && !core.cancel.load(Ordering::Relaxed) {
                    process_pending(core, sink, true);
                    thread::sleep(Duration::from_millis(700));
                }
            });
            for (id, path) in &folders {
                if core.cancel.load(Ordering::Relaxed) { break; }
                sink.allow_path(Path::new(path));
                if let Err(e) = discover(core, sink, *id, Path::new(path)) {
                    eprintln!("gallerydesk: scan of {path} failed: {e}");
                }
                sink.changed();
            }
            walking.store(false, Ordering::Relaxed);
        });
    }

    if !core.cancel.load(Ordering::Relaxed) {
        process_pending(core, sink, false);
    }

    let cancelled = core.cancel.load(Ordering::Relaxed);
    publish(core, sink, |p| {
        p.running = false;
        p.cancelled = cancelled;
        p.phase = "idle".into();
    });
    sink.changed();
}

type NewKey = (String, i64, i64);

fn discover(core: &Core, sink: &dyn Sink, folder_id: i64, root: &Path) -> Result<(), String> {
    if !root.is_dir() {
        // Unplugged drive or deleted folder: keep its catalog entries, do not purge.
        return Ok(());
    }
    let conn = core.db.open().map_err(|e| e.to_string())?;
    let rules = crate::exclude::load(&conn);
    if rules.excludes_dir(root) { return Ok(()); } // the user excluded this very folder
    let gen = {
        let t = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(1);
        t.max(1)
    };
    let started = now_secs();
    let folder_label = root.file_name().and_then(|n| n.to_str()).unwrap_or("folder").to_string();
    let mut discovered = 0u64;
    let mut walk_errors: Vec<PathBuf> = Vec::new();
    let mut fresh: HashMap<NewKey, i64> = HashMap::new();

    conn.execute_batch("BEGIN").map_err(|e| e.to_string())?;
    let mut in_tx = 0u32;

    for entry in WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| !is_hidden(e) && !(e.depth() > 0 && e.file_type().is_dir() && rules.excludes_dir(e.path()))) {
        if core.cancel.load(Ordering::Relaxed) { break; }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                if let Some(p) = e.path() { walk_errors.push(p.to_path_buf()); }
                continue;
            }
        };
        if !entry.file_type().is_file() { continue; }
        let path = entry.path();
        let Some(kind) = media_kind(path) else { continue };
        let Ok(md) = fs::metadata(path) else { continue };
        let bytes = md.len() as i64;
        let modified = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
        let path_s = path.to_string_lossy().to_string();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("Untitled").to_string();
        let dir = path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();

        let existing: Option<(i64, i64, i64, Option<String>)> = conn
            .prepare_cached("SELECT id,bytes,modified,thumb FROM media WHERE path=?1")
            .and_then(|mut s| s.query_row([&path_s], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional())
            .map_err(|e| e.to_string())?;

        match existing {
            Some((id, b, m, _)) if b == bytes && m == modified => {
                conn.prepare_cached("UPDATE media SET seen=?1 WHERE id=?2")
                    .and_then(|mut s| s.execute(params![gen, id]))
                    .map_err(|e| e.to_string())?;
            }
            Some((id, _, _, old_thumb)) => {
                remove_thumb(core, &old_thumb);
                conn.prepare_cached(
                    "UPDATE media SET bytes=?1,modified=?2,sort_date=?2,taken=NULL,thumb=NULL,
                     meta_state=0,hash=NULL,seen=?3,folder_id=?4 WHERE id=?5",
                )
                .and_then(|mut s| s.execute(params![bytes, modified, gen, folder_id, id]))
                .map_err(|e| e.to_string())?;
            }
            None => {
                conn.prepare_cached(
                    "INSERT INTO media(folder_id,path,dir,name,kind,bytes,modified,added,sort_date,seen)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?7,?9)",
                )
                .and_then(|mut s| s.execute(params![folder_id, path_s, dir, name, kind, bytes, modified, started, gen]))
                .map_err(|e| e.to_string())?;
                fresh.insert((name, bytes, modified), conn.last_insert_rowid());
            }
        }

        discovered += 1;
        in_tx += 1;
        if in_tx >= 40 {
            conn.execute_batch("COMMIT; BEGIN").map_err(|e| e.to_string())?;
            in_tx = 0;
        }
        if discovered % 200 == 0 {
            publish(core, sink, |p| {
                p.phase = "discover".into();
                p.folder = folder_label.clone();
                p.discovered = discovered;
            });
        }
    }
    conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;
    publish(core, sink, |p| { p.discovered = discovered; p.folder = folder_label.clone(); });

    if core.cancel.load(Ordering::Relaxed) {
        return Ok(()); // `seen` is incomplete after a cancel, so never purge on this path.
    }
    reconcile(core, &conn, folder_id, gen, &walk_errors, &mut fresh)?;
    conn.execute("UPDATE folders SET last_scan=?1 WHERE id=?2", params![now_secs(), folder_id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Remove catalog rows whose files are gone. A row that vanished while an identical file
/// (same name, size, mtime) appeared elsewhere in the same root is treated as a move and
/// its favorite flag, rotation, album membership and tags are carried over.
fn reconcile(
    core: &Core,
    conn: &Connection,
    folder_id: i64,
    gen: i64,
    walk_errors: &[PathBuf],
    fresh: &mut HashMap<NewKey, i64>,
) -> Result<(), String> {
    #[allow(clippy::type_complexity)]
    let stale: Vec<(i64, String, String, i64, i64, Option<String>, i64, i64)> = {
        let mut s = conn
            .prepare("SELECT id,path,name,bytes,modified,thumb,favorite,rotation FROM media WHERE folder_id=?1 AND seen<>?2")
            .map_err(|e| e.to_string())?;
        let rows = s
            .query_map(params![folder_id, gen], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?
    };
    conn.execute_batch("BEGIN").map_err(|e| e.to_string())?;
    for (id, path, name, bytes, modified, thumb, favorite, rotation) in stale {
        // Could not read part of the tree (permissions): do not treat its files as deleted.
        if walk_errors.iter().any(|p| Path::new(&path).starts_with(p)) { continue; }
        if let Some(new_id) = fresh.remove(&(name, bytes, modified)) {
            let _ = conn.execute("UPDATE media SET favorite=?1, rotation=?2 WHERE id=?3", params![favorite, rotation, new_id]);
            let _ = conn.execute("UPDATE OR IGNORE album_items SET media_id=?1 WHERE media_id=?2", params![new_id, id]);
            let _ = conn.execute("UPDATE OR IGNORE media_tags SET media_id=?1 WHERE media_id=?2", params![new_id, id]);
            if rotation != 0 {
                let _ = conn.execute("UPDATE media SET meta_state=0 WHERE id=?1", [new_id]);
            }
        }
        remove_thumb(core, &thumb);
        conn.execute("DELETE FROM media WHERE id=?1", [id]).map_err(|e| e.to_string())?;
    }
    conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;
    Ok(())
}

struct Job {
    id: i64,
    path: String,
    kind: String,
    bytes: i64,
    modified: i64,
    rotation: i64,
}

pub fn worker_count(conn: &Connection) -> usize {
    let auto = thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let auto = auto.saturating_sub(1).clamp(1, 6);
    setting(conn, "workers").and_then(|v| v.parse::<usize>().ok()).filter(|n| (1..=8).contains(n)).unwrap_or(auto)
}

fn thumb_size(conn: &Connection) -> u32 {
    setting(conn, "thumb_size").and_then(|v| v.parse::<u32>().ok()).filter(|n| (160..=1024).contains(n)).unwrap_or(480)
}

fn process_pending(core: &Core, sink: &dyn Sink, quiet: bool) {
    let Ok(conn) = core.db.open() else { return };
    let jobs: Vec<Job> = {
        let Ok(mut s) = conn.prepare(
            "SELECT id,path,kind,bytes,modified,rotation FROM media WHERE meta_state=0 ORDER BY (rotation<>0) DESC, sort_date DESC",
        ) else { return };
        let rows = s.query_map([], |r| {
            Ok(Job { id: r.get(0)?, path: r.get(1)?, kind: r.get(2)?, bytes: r.get(3)?, modified: r.get(4)?, rotation: r.get(5)? })
        });
        match rows {
            Ok(r) => r.filter_map(Result::ok).collect(),
            Err(_) => return,
        }
    };
    let workers = worker_count(&conn);
    let size = thumb_size(&conn);
    drop(conn);
    let total = jobs.len() as u64;
    if !quiet { publish(core, sink, |p| { p.phase = "process".into(); p.total = total; p.processed = 0; p.failed = 0; }); }
    if jobs.is_empty() { return; }

    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let alive = AtomicUsize::new(workers.min(jobs.len()));
    let has_ffprobe = media::tool_available("ffprobe");

    thread::scope(|scope| {
        for _ in 0..workers.min(jobs.len()) {
            scope.spawn(|| {
                if let Ok(conn) = core.db.open() {
                    loop {
                        if core.cancel.load(Ordering::Relaxed) { break; }
                        // A newer request (new/removed file, rotation) must not wait behind a long
                        // preview run. Stop here; unfinished rows stay queued and resume afterwards.
                        if core.pending.lock().unwrap().is_some() { break; }
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        let Some(job) = jobs.get(i) else { break };
                        let ok = process_one(core, &conn, job, size, has_ffprobe);
                        if !ok { failed.fetch_add(1, Ordering::Relaxed); }
                        done.fetch_add(1, Ordering::Relaxed);
                    }
                }
                alive.fetch_sub(1, Ordering::SeqCst);
            });
        }
        let mut last_changed = Instant::now();
        let mut last_done = 0usize;
        while alive.load(Ordering::SeqCst) > 0 {
            thread::sleep(Duration::from_millis(200));
            let d = done.load(Ordering::Relaxed);
            if !quiet { publish(core, sink, |p| { p.processed = d as u64; p.failed = failed.load(Ordering::Relaxed) as u64; }); }
            if d != last_done && last_changed.elapsed() > Duration::from_millis(900) {
                sink.changed();
                last_changed = Instant::now();
                last_done = d;
            }
        }
    });
    let d = done.load(Ordering::Relaxed) as u64;
    if !quiet { publish(core, sink, |p| { p.processed = d; p.failed = failed.load(Ordering::Relaxed) as u64; }); }
    else if d > 0 { sink.changed(); }
}

/// Extract metadata and render the thumbnail for one catalog row.
fn process_one(core: &Core, conn: &Connection, job: &Job, size: u32, has_ffprobe: bool) -> bool {
    let path = Path::new(&job.path);
    let is_video = job.kind == "video";
    let mut meta = if is_video { Meta { orientation: 1, ..Default::default() } } else { media::read_exif(path) };
    if is_video || meta.width.is_none() || meta.height.is_none() {
        if has_ffprobe {
            if let Some(p) = media::probe(path) {
                meta.width = meta.width.or(p.width);
                meta.height = meta.height.or(p.height);
                if is_video { meta.duration = p.duration; } // ffprobe reports ~0.04s for still images
                meta.taken = meta.taken.or(p.taken);
            }
        }
    }

    let key = cache_key(&job.path, job.bytes, job.modified, job.rotation, size);
    let name = format!("{key}.jpg");
    let out = core.thumbs_dir.join(&name);
    let thumb_ok = out.exists() || media::render(
        &RenderOpts {
            src: path,
            out: &out,
            long_edge: size,
            video: is_video,
            orientation: meta.orientation,
            rotation: job.rotation,
            seek: if is_video { meta.duration.map(|d| (d * 0.15).min(5.0)) } else { None },
            quality: 4,
        },
        &core.cancel,
    );
    if core.cancel.load(Ordering::Relaxed) { return false; } // leave row pending for next run

    let state = if thumb_ok { 1 } else { 2 };
    let res = conn.execute(
        "UPDATE media SET taken=?1, sort_date=COALESCE(?1, modified), width=?2, height=?3, duration=?4,
         camera=?5, lens=?6, iso=?7, aperture=?8, exposure=?9, focal=?10, lat=?11, lon=?12,
         thumb=?13, meta_state=?14 WHERE id=?15 AND modified=?16 AND bytes=?17",
        params![
            meta.taken, meta.width, meta.height, meta.duration, meta.camera, meta.lens, meta.iso,
            meta.aperture, meta.exposure, meta.focal, meta.lat, meta.lon,
            if thumb_ok { Some(name) } else { None }, state, job.id, job.modified, job.bytes
        ],
    );
    if let Err(e) = res { eprintln!("gallerydesk: could not save metadata for {}: {e}", job.path); }
    thumb_ok
}

/// Drain the queue synchronously (used by tests and by one-shot callers).
pub fn run_blocking(core: &Core, sink: &dyn Sink, req: Request) {
    core.running.store(true, Ordering::SeqCst);
    run_job(core, sink, req);
    core.running.store(false, Ordering::SeqCst);
}

