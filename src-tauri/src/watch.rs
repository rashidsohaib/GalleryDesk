//! Optional filesystem watcher. It does nothing but note "something changed under root N";
//! a single debounce thread then asks the scanner for an incremental pass once the folder
//! has been quiet for a few seconds. When disabled, no watcher and no thread exist.

use crate::scan::{self, Core, Request, Sink};
use notify::{event::ModifyKind, EventKind, RecursiveMode, Watcher as _};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct WatchState {
    inner: Mutex<Option<Active>>,
}

struct Active {
    _watchers: Vec<notify::RecommendedWatcher>,
    stop: Arc<AtomicBool>,
}

impl Drop for Active {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn relevant(kind: &EventKind, paths: &[PathBuf]) -> bool {
    let kind_ok = matches!(
        kind,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) | EventKind::Modify(ModifyKind::Data(_))
    );
    if !kind_ok { return false; }
    paths.iter().any(|p| {
        let hidden = p.components().any(|c| c.as_os_str().to_str().map(|s| s.starts_with('.')).unwrap_or(false));
        !hidden && (p.extension().is_none() || crate::media::media_kind(p).is_some())
    })
}

impl WatchState {
    /// (Re)start watching every root, or stop everything when `enabled` is false.
    pub fn restart(&self, core: &Arc<Core>, sink: &Arc<dyn Sink>, enabled: bool) {
        let mut guard = self.inner.lock().unwrap();
        *guard = None; // drops watchers and stops the old debounce thread
        if !enabled { return; }

        let roots: Vec<(i64, String)> = match core.db.open().and_then(|c| {
            let mut s = c.prepare("SELECT id,path FROM folders")?;
            let rows = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<Vec<_>, _>>()
        }) {
            Ok(r) => r,
            Err(_) => return,
        };

        let dirty: Arc<Mutex<HashMap<i64, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        let mut watchers = Vec::new();
        for (id, path) in roots {
            if !Path::new(&path).is_dir() { continue; }
            let d = dirty.clone();
            let w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(ev) = res {
                    if relevant(&ev.kind, &ev.paths) {
                        d.lock().unwrap().insert(id, Instant::now());
                    }
                }
            });
            match w {
                Ok(mut w) => {
                    // A failure here is usually the inotify watch limit on a huge tree.
                    // Fall back to rescan-on-launch / manual rescan for that folder.
                    if w.watch(Path::new(&path), RecursiveMode::Recursive).is_ok() {
                        watchers.push(w);
                    } else {
                        eprintln!("gallerydesk: could not watch {path} (inotify limit?)");
                    }
                }
                Err(e) => eprintln!("gallerydesk: watcher unavailable: {e}"),
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let (core2, sink2, stop2) = (core.clone(), sink.clone(), stop.clone());
        thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1000));
                let ready: HashSet<i64> = {
                    let mut m = dirty.lock().unwrap();
                    let ids: Vec<i64> = m.iter().filter(|(_, t)| t.elapsed() > Duration::from_secs(4)).map(|(i, _)| *i).collect();
                    for i in &ids { m.remove(i); }
                    ids.into_iter().collect()
                };
                if !ready.is_empty() && !stop2.load(Ordering::SeqCst) {
                    scan::request(&core2, &sink2, Request { folders: Some(ready), discover: true, retry_failed: false });
                }
            }
        });
        *guard = Some(Active { _watchers: watchers, stop });
    }
}
