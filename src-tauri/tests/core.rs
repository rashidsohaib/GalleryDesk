//! Integration tests for the indexing engine and catalog, run against real files and the real
//! ffmpeg/ffprobe on PATH. They use no Tauri runtime.

use gallerydesk_lib::{exclude, media, query, stream, scan::{self, Core, Progress, Request, Sink}};
use query::{Filter, Sort};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{atomic::{AtomicBool, Ordering}, Mutex},
};
use tempfile::TempDir;

#[derive(Default)]
struct Rec { events: Mutex<Vec<Progress>>, changed: Mutex<u32> }
impl Sink for Rec {
    fn progress(&self, p: &Progress) { self.events.lock().unwrap().push(p.clone()); }
    fn changed(&self) { *self.changed.lock().unwrap() += 1; }
}

fn fixtures() -> PathBuf { Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures") }

struct Env { _data: TempDir, _lib: TempDir, core: Core, root: PathBuf, rec: Rec, folder: i64 }

fn setup() -> Env {
    let data = tempfile::tempdir().unwrap();
    let lib = tempfile::tempdir().unwrap();
    let root = lib.path().join("photos");
    fs::create_dir_all(root.join("subdir")).unwrap();
    fs::create_dir_all(root.join(".hidden")).unwrap();
    for f in ["exif_orient6.jpg", "plain.png", "fake_raw.dng", "corrupt.jpg", "clip.mp4"] {
        fs::copy(fixtures().join(f), root.join(f)).unwrap();
    }
    fs::copy(fixtures().join("plain.png"), root.join("subdir/plain.png")).unwrap();
    fs::copy(fixtures().join("plain.png"), root.join(".hidden/secret.png")).unwrap();
    fs::write(root.join("notes.txt"), "not media").unwrap();
    let core = Core::new(data.path().to_path_buf()).unwrap();
    let conn = core.db.open().unwrap();
    let folder = query::add_folder(&conn, root.to_str().unwrap()).unwrap();
    Env { _data: data, _lib: lib, core, root, rec: Rec::default(), folder }
}

fn scan_all(e: &Env) {
    scan::run_blocking(&e.core, &e.rec, Request { folders: None, discover: true, retry_failed: false });
}

fn all() -> (Filter, Sort) { (Filter { kind: "all".into(), scope: "all".into(), ..Default::default() }, Sort { key: "name".into(), desc: false }) }

fn rows(e: &Env) -> Vec<query::MediaItem> {
    let (f, s) = all();
    query::query_media(&e.core.db.open().unwrap(), &e.core.thumbs_dir, &f, &s, 0, 1000).unwrap()
}
fn by_name(e: &Env, name: &str) -> query::MediaItem { rows(e).into_iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no row {name}")) }

#[test]
fn exif_fields_are_extracted() {
    let m = media::read_exif(&fixtures().join("exif_orient6.jpg"));
    assert_eq!(m.orientation, 6);
    // 400x200 stored, orientation 6 => displayed 200x400
    assert_eq!((m.width, m.height), (Some(200), Some(400)));
    assert_eq!(m.taken, Some(1625441415)); // 2021-07-04 23:30:15 as wall-clock-in-UTC
    assert_eq!(m.camera.as_deref(), Some("Canon EOS R5")); // make not duplicated
    assert_eq!(m.iso, Some(400));
    assert_eq!(m.aperture, Some(2.8));
    assert_eq!(m.exposure.as_deref(), Some("1/250s"));
    assert_eq!(m.focal, Some(50.0));
    assert_eq!(m.lens.as_deref(), Some("RF50mm F1.8"));
    assert!((m.lat.unwrap() + 33.8667).abs() < 0.001, "south is negative: {:?}", m.lat);
    assert!((m.lon.unwrap() - 151.2).abs() < 0.001);
}

#[test]
fn full_pipeline_indexes_everything_and_skips_junk() {
    let e = setup();
    scan_all(&e);
    let items = rows(&e);
    let names: Vec<_> = items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(items.len(), 6, "{names:?}"); // no .txt, no hidden dir
    assert!(!names.contains(&"secret.png") && !names.contains(&"notes.txt"));

    // corrupt.jpg cannot be decoded: honest state 2, no thumbnail. Everything else is ready.
    for it in &items {
        if it.name == "corrupt.jpg" { assert_eq!(it.state, 2); assert!(it.thumbnail.is_none()); }
        else {
            assert_eq!(it.state, 1, "{} not ready", it.name);
            assert!(Path::new(it.thumbnail.as_ref().unwrap()).exists(), "{} thumb missing", it.name);
        }
    }
    // Fake RAW only succeeds via the embedded-JPEG fallback.
    assert_eq!(by_name(&e, "fake_raw.dng").state, 1);

    // EXIF orientation is applied to the thumbnail: stored 400x200 -> portrait 200x400.
    let t = media::probe(Path::new(by_name(&e, "exif_orient6.jpg").thumbnail.as_ref().unwrap())).unwrap();
    assert_eq!((t.width, t.height), (Some(200), Some(400)));

    // Video metadata + poster frame.
    let conn = e.core.db.open().unwrap();
    let d = query::get_media(&conn, &e.core.thumbs_dir, by_name(&e, "clip.mp4").id).unwrap();
    assert_eq!((d.item.width, d.item.height), (Some(320), Some(180)));
    assert!((d.item.duration.unwrap() - 2.0).abs() < 0.3);

    // sort_date uses the EXIF capture date, not the file mtime.
    let d = query::get_media(&conn, &e.core.thumbs_dir, by_name(&e, "exif_orient6.jpg").id).unwrap();
    assert_eq!(d.item.duration, None, "stills must not get a duration from ffprobe");
    assert_eq!(d.taken, Some(1625441415));
    assert_eq!(d.item.date, 1625441415);

    // Progress reporting reached the process phase and finished idle.
    let ev = e.rec.events.lock().unwrap();
    assert!(ev.iter().any(|p| p.phase == "discover" && p.running));
    assert!(ev.iter().any(|p| p.phase == "process" && p.total == 6));
    let last = ev.last().unwrap();
    assert!(!last.running && last.phase == "idle" && last.processed == 6 && last.failed == 1);
}

#[test]
fn rescan_is_incremental_and_reconciles_changes() {
    let e = setup();
    scan_all(&e);
    let before = by_name(&e, "plain.png");
    let thumb_before = before.thumbnail.clone().unwrap();

    // Nothing changed: same rows, same thumbnail, nothing queued for processing.
    e.rec.events.lock().unwrap().clear();
    scan_all(&e);
    assert_eq!(by_name(&e, "plain.png").thumbnail.as_deref(), Some(thumb_before.as_str()));
    assert!(e.rec.events.lock().unwrap().iter().all(|p| p.total == 0), "unchanged files must not be reprocessed");

    // Deleted file: row and cached thumbnail disappear. The original dir is not touched.
    let gone_thumb = by_name(&e, "clip.mp4").thumbnail.unwrap();
    fs::remove_file(e.root.join("clip.mp4")).unwrap();
    // Changed file (different bytes): thumbnail is regenerated under a new cache key.
    let img = image_bytes(&e.root.join("subdir/plain.png"));
    fs::write(e.root.join("subdir/plain.png"), img).unwrap();
    scan_all(&e);
    assert!(rows(&e).iter().all(|r| r.name != "clip.mp4"));
    assert!(!Path::new(&gone_thumb).exists(), "stale thumbnail must be removed");
    assert_eq!(rows(&e).len(), 5);
    assert!(e.root.join("subdir").is_dir());
}

fn image_bytes(p: &Path) -> Vec<u8> { let mut b = fs::read(p).unwrap(); b.extend_from_slice(b"changed"); b }

#[test]
fn changed_file_gets_new_thumbnail_key() {
    let e = setup();
    scan_all(&e);
    let id = by_name(&e, "plain.png").id;
    let old = query::get_media(&e.core.db.open().unwrap(), &e.core.thumbs_dir, id).unwrap().item.thumbnail.unwrap();
    // Re-encode to a different image at the same path (bumps size and mtime).
    let status = std::process::Command::new("ffmpeg").args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=100x80", "-frames:v", "1"])
        .arg(e.root.join("plain.png")).status().unwrap();
    assert!(status.success());
    scan_all(&e);
    let now = by_name(&e, "plain.png");
    assert_ne!(now.thumbnail.as_deref(), Some(old.as_str()), "content change at same path must not reuse the old thumbnail");
    assert!(!Path::new(&old).exists());
    let t = media::probe(Path::new(now.thumbnail.as_ref().unwrap())).unwrap();
    assert_eq!((t.width, t.height), (Some(100), Some(80)));
}

#[test]
fn moved_file_keeps_favorite_album_and_tags() {
    let e = setup();
    scan_all(&e);
    let c = e.core.db.open().unwrap();
    let id = by_name(&e, "exif_orient6.jpg").id;
    query::set_favorite(&c, &[id], true).unwrap();
    let album = query::create_album(&c, "Sydney").unwrap();
    query::album_add(&c, album, &[id]).unwrap();
    query::add_tag(&c, &[id], "holiday").unwrap();

    fs::create_dir_all(e.root.join("moved")).unwrap();
    fs::rename(e.root.join("exif_orient6.jpg"), e.root.join("moved/renamed_dir_copy.jpg")).unwrap();
    // A pure move keeps the name; do the real thing: same name, new directory.
    fs::rename(e.root.join("moved/renamed_dir_copy.jpg"), e.root.join("moved/exif_orient6.jpg")).unwrap();
    scan_all(&e);

    let list: Vec<_> = rows(&e).into_iter().filter(|r| r.name == "exif_orient6.jpg").collect();
    assert_eq!(list.len(), 1, "no duplicate row after a move");
    assert!(list[0].path.contains("/moved/"));
    assert!(list[0].favorite, "favorite must survive a move");
    let d = query::get_media(&c, &e.core.thumbs_dir, list[0].id).unwrap();
    assert_eq!(d.tags, vec!["holiday"]);
    assert_eq!(d.albums.len(), 1);
    assert_eq!(query::list_albums(&c).unwrap()[0].count, 1);
}

#[test]
fn unavailable_root_is_not_purged() {
    let e = setup();
    scan_all(&e);
    let parked = e.root.with_file_name("photos_unplugged");
    fs::rename(&e.root, &parked).unwrap();
    scan_all(&e);
    assert_eq!(rows(&e).len(), 6, "an unmounted drive must not wipe its catalog entries");
    let folders = query::list_folders(&e.core.db.open().unwrap()).unwrap();
    assert!(!folders[0].available);
    fs::rename(&parked, &e.root).unwrap();
    scan_all(&e);
    assert_eq!(rows(&e).len(), 6);
}

#[test]
fn cancel_before_discovery_changes_nothing() {
    let e = setup();
    scan_all(&e);
    fs::remove_file(e.root.join("plain.png")).unwrap();
    e.core.cancel.store(true, Ordering::SeqCst);
    scan_all(&e);
    // A cancelled pass has an incomplete `seen` set, so it must never purge.
    assert_eq!(rows(&e).len(), 6);
    e.core.cancel.store(false, Ordering::SeqCst);
    scan_all(&e);
    assert_eq!(rows(&e).len(), 5);
}

#[test]
fn filters_sorting_paging_and_layout() {
    let e = setup();
    scan_all(&e);
    let c = e.core.db.open().unwrap();
    let t = &e.core.thumbs_dir;
    let (mut f, s) = all();

    assert_eq!(query::query_layout(&c, &f, "none", true).unwrap().total, 6);
    f.search = "PLAIN".into(); // case-insensitive
    assert_eq!(query::query_media(&c, t, &f, &s, 0, 10).unwrap().len(), 2);
    f.search = "50%_".into(); // LIKE wildcards are escaped, not interpreted
    assert_eq!(query::query_layout(&c, &f, "none", true).unwrap().total, 0);
    f.search = "canon".into(); // camera is searchable
    assert_eq!(query::query_layout(&c, &f, "none", true).unwrap().total, 1);

    let (mut f, _) = all();
    f.kind = "video".into();
    assert_eq!(query::query_layout(&c, &f, "none", true).unwrap().total, 1);

    // name ordering + paging
    let (f, _) = all();
    let asc = Sort { key: "name".into(), desc: false };
    let names: Vec<_> = query::query_media(&c, t, &f, &asc, 0, 100).unwrap().into_iter().map(|i| i.name).collect();
    assert_eq!(names, ["clip.mp4", "corrupt.jpg", "exif_orient6.jpg", "fake_raw.dng", "plain.png", "plain.png"]);
    let page: Vec<_> = query::query_media(&c, t, &f, &asc, 2, 2).unwrap().into_iter().map(|i| i.name).collect();
    assert_eq!(page, ["exif_orient6.jpg", "fake_raw.dng"]);
    let ids = query::query_ids(&c, &f, &asc, 1, 3).unwrap();
    assert_eq!(ids.len(), 3);

    // Timeline layout: group counts add up to the total and respect date order.
    let date_desc = Sort { key: "date".into(), desc: true };
    let lay = query::query_layout(&c, &f, "month", true).unwrap();
    assert_eq!(lay.groups.iter().map(|g| g.count).sum::<i64>(), lay.total);
    assert!(lay.groups.iter().any(|g| g.key == "2021-07" && g.count == 1));
    // Regression: headers must follow the item order. Newest first => 2021-07 is the LAST group,
    // and the first group's count equals the number of leading rows that belong to it.
    assert_eq!(lay.groups.last().unwrap().key, "2021-07");
    let keys: Vec<_> = lay.groups.iter().map(|g| g.key.clone()).collect();
    let mut sorted = keys.clone(); sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(keys, sorted);
    assert_eq!(query::query_layout(&c, &f, "month", false).unwrap().groups[0].key, "2021-07");
    // Row boundaries: every item inside group k must really fall in that month.
    let all_desc = query::query_media(&c, t, &f, &date_desc, 0, 100).unwrap();
    let mut idx = 0usize;
    for g in &lay.groups {
        for it in &all_desc[idx..idx + g.count as usize] {
            let (y, m) = ymd(it.date);
            assert_eq!(format!("{y:04}-{m:02}"), g.key, "{} sits under the wrong header", it.name);
        }
        idx += g.count as usize;
    }
    assert_eq!(idx, all_desc.len());
    let first = query::query_media(&c, t, &f, &date_desc, 0, 1).unwrap();
    assert_ne!(first[0].name, "exif_orient6.jpg", "2021 photo sorts after recent files");

    // folder / sub-directory scope
    let (mut f, _) = all();
    f.scope = "folder".into(); f.folder_id = Some(e.folder); f.dir = Some(e.root.join("subdir").to_string_lossy().into());
    assert_eq!(query::query_layout(&c, &f, "none", true).unwrap().total, 1);
    let dirs = query::list_dirs(&c, e.folder).unwrap();
    assert_eq!(dirs.len(), 2);

    // favorites / tags / albums / smart
    let id = by_name(&e, "plain.png").id;
    query::set_favorite(&c, &[id], true).unwrap();
    let mut fav = Filter { scope: "favorites".into(), ..Default::default() };
    assert_eq!(query::query_layout(&c, &fav, "none", true).unwrap().total, 1);
    query::add_tag(&c, &[id], "Beach").unwrap();
    query::add_tag(&c, &[id], "beach").unwrap(); // case-insensitive: same tag
    assert_eq!(query::list_tags(&c).unwrap().len(), 1);
    fav = Filter { scope: "tag".into(), tag: Some("BEACH".into()), ..Default::default() };
    assert_eq!(query::query_layout(&c, &fav, "none", true).unwrap().total, 1);
    let untagged = Filter { scope: "smart".into(), smart: Some("untagged".into()), ..Default::default() };
    assert_eq!(query::query_layout(&c, &untagged, "none", true).unwrap().total, 5);
    let gps = Filter { scope: "smart".into(), smart: Some("gps".into()), ..Default::default() };
    assert_eq!(query::query_layout(&c, &gps, "none", true).unwrap().total, 1);
    query::remove_tag(&c, &[id], "beach").unwrap();
    assert!(query::list_tags(&c).unwrap().is_empty(), "orphan tags are cleaned up");
}

#[test]
fn rotation_is_non_destructive_and_rerenders() {
    let e = setup();
    scan_all(&e);
    let c = e.core.db.open().unwrap();
    let id = by_name(&e, "plain.png").id; // 64x48
    let original = fs::read(e.root.join("plain.png")).unwrap();
    query::rotate(&c, &e.core.thumbs_dir, &[id], 90).unwrap();
    scan::run_blocking(&e.core, &e.rec, Request { folders: Some(Default::default()), discover: false, retry_failed: false });
    let it = by_name(&e, "plain.png");
    assert_eq!(it.rotation, 90);
    let t = media::probe(Path::new(it.thumbnail.as_ref().unwrap())).unwrap();
    assert_eq!((t.width, t.height), (Some(48), Some(64)));
    query::rotate(&c, &e.core.thumbs_dir, &[id], -180).unwrap(); // 90-180 = 270
    assert_eq!(by_name(&e, "plain.png").rotation, 270);
    assert_eq!(fs::read(e.root.join("plain.png")).unwrap(), original, "original untouched");
}

#[test]
fn duplicates_are_found_by_content() {
    let e = setup();
    scan_all(&e);
    let c = e.core.db.open().unwrap();
    let groups = query::find_duplicates(&c, &e.core.thumbs_dir, &AtomicBool::new(false), |_, _| {}).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].items.len(), 2);
    assert!(groups[0].items.iter().all(|i| i.name == "plain.png"));
    // Same size but different content is not a duplicate.
    let mut b = fs::read(e.root.join("subdir/plain.png")).unwrap();
    let last = b.len() - 1; b[last] ^= 0xFF;
    let f = e.root.join("subdir/plain.png");
    fs::write(&f, b).unwrap();
    // mtime is tracked in whole seconds; make the edit unambiguous instead of racing the clock.
    fs::File::options().write(true).open(&f).unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
    scan_all(&e);
    assert!(query::find_duplicates(&c, &e.core.thumbs_dir, &AtomicBool::new(false), |_, _| {}).unwrap().is_empty());
}

#[test]
fn folder_management_rules() {
    let e = setup();
    let c = e.core.db.open().unwrap();
    assert_eq!(query::add_folder(&c, e.root.to_str().unwrap()).unwrap(), e.folder, "same folder is idempotent");
    assert!(query::add_folder(&c, e.root.join("subdir").to_str().unwrap()).unwrap_err().contains("Already covered"));
    assert!(query::add_folder(&c, e.root.parent().unwrap().to_str().unwrap()).unwrap_err().contains("contains"));
    assert!(query::add_folder(&c, "/definitely/not/here").is_err());
    scan_all(&e);
    let thumbs_before = fs::read_dir(&e.core.thumbs_dir).unwrap().count();
    assert!(thumbs_before > 0);
    query::remove_folder(&c, &e.core.thumbs_dir, e.folder).unwrap();
    assert_eq!(query::library_stats(&c).unwrap().total, 0);
    assert_eq!(fs::read_dir(&e.core.thumbs_dir).unwrap().count(), 0, "cache cleaned");
    assert!(e.root.join("plain.png").exists(), "originals are never deleted");
}

#[test]
fn albums_settings_and_validation() {
    let e = setup();
    let c = e.core.db.open().unwrap();
    assert!(query::create_album(&c, "  ").is_err());
    let a = query::create_album(&c, "Trips").unwrap();
    assert!(query::create_album(&c, "trips").unwrap_err().contains("already exists"));
    let b = query::create_album(&c, "Family").unwrap();
    assert!(query::rename_album(&c, b, "TRIPS").is_err());
    query::rename_album(&c, b, "Family 2026").unwrap();
    query::delete_album(&c, a).unwrap();
    assert_eq!(query::list_albums(&c).unwrap().len(), 1);

    let s = query::get_settings(&c).unwrap();
    assert_eq!(s["theme"], "system");
    query::set_setting(&c, "theme", "light").unwrap();
    assert_eq!(query::get_settings(&c).unwrap()["theme"], "light");
    assert!(query::set_setting(&c, "bogus", "1").is_err());
}

#[test]
fn backup_roundtrip_and_validation() {
    let e = setup();
    scan_all(&e);
    let c = e.core.db.open().unwrap();
    let a = query::create_album(&c, "Keep me").unwrap();
    let id = by_name(&e, "plain.png").id;
    query::album_add(&c, a, &[id]).unwrap();
    let dest = e._data.path().join("backup.sqlite3");
    query::backup_catalog(&c, &dest).unwrap();
    query::backup_catalog(&c, &dest).unwrap(); // overwrite is fine
    query::validate_backup(&dest).unwrap();
    fs::write(e._data.path().join("junk.sqlite3"), b"nope").unwrap();
    assert!(query::validate_backup(&e._data.path().join("junk.sqlite3")).is_err());

    // Restored catalog whose thumbnails were wiped re-queues them instead of showing broken images.
    for f in fs::read_dir(&e.core.thumbs_dir).unwrap() { fs::remove_file(f.unwrap().path()).unwrap(); }
    let n = query::requeue_missing_thumbs(&c, &e.core.thumbs_dir).unwrap();
    assert_eq!(n, 5);
    assert_eq!(query::library_stats(&c).unwrap().pending, 5);
}

#[test]
fn legacy_starter_database_is_migrated() {
    let data = tempfile::tempdir().unwrap();
    fs::create_dir_all(data.path().join("thumbnails")).unwrap();
    fs::write(data.path().join("thumbnails/deadbeef.jpg"), b"old").unwrap();
    {
        let c = rusqlite::Connection::open(data.path().join("library.sqlite3")).unwrap();
        c.execute_batch("CREATE TABLE media (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, name TEXT NOT NULL, kind TEXT NOT NULL, bytes INTEGER, modified INTEGER, thumbnail TEXT); INSERT INTO media(path,name,kind) VALUES('/x/a.jpg','a.jpg','image');").unwrap();
    }
    let core = Core::new(data.path().to_path_buf()).expect("0.1.0 catalogs must open");
    let c = core.db.open().unwrap();
    assert_eq!(query::library_stats(&c).unwrap().total, 0);
    assert!(!data.path().join("thumbnails/deadbeef.jpg").exists());
    // reopening an up-to-date catalog is a no-op
    Core::new(data.path().to_path_buf()).unwrap();
}

#[test]
fn embedded_jpeg_extraction() {
    let jpg = media::extract_embedded_jpeg(&fixtures().join("fake_raw.dng")).expect("embedded preview");
    assert_eq!(&jpg[..3], &[0xFF, 0xD8, 0xFF]);
    assert_eq!(&jpg[jpg.len() - 2..], &[0xFF, 0xD9]);
    assert!(media::extract_embedded_jpeg(&fixtures().join("corrupt.jpg")).is_none());
}

#[test]
fn queued_requests_merge_while_running() {
    use std::sync::Arc;
    let e = setup();
    let Env { _data, _lib, core, rec, folder, .. } = e;
    let core = Arc::new(core);
    let sink: Arc<dyn Sink> = Arc::new(rec);
    for _ in 0..5 {
        scan::request(&core, &sink, Request { folders: None, discover: true, retry_failed: false });
    }
    let t0 = std::time::Instant::now();
    while core.running.load(Ordering::SeqCst) || core.pending.lock().unwrap().is_some() {
        assert!(t0.elapsed().as_secs() < 60, "scan never finished");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let c = core.db.open().unwrap();
    assert_eq!(query::library_stats(&c).unwrap().total, 6);
    let _ = folder;
}

fn ymd(secs: i64) -> (i64, i64) {
    // civil-from-days (Hinnant)
    let z = secs.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m)
}

#[test]
fn excluded_folders_are_skipped_purged_and_restorable() {
    let e = setup();
    scan_all(&e);
    assert!(rows(&e).iter().any(|r| r.path.contains("/subdir/")), "subdir indexed before exclusion");
    let c = e.core.db.open().unwrap();
    assert!(exclude::add(&c, &e.core.thumbs_dir, "relative/path/x").is_err(), "relative paths with slashes are rejected");
    assert!(exclude::add(&c, &e.core.thumbs_dir, "  ").is_err());
    assert!(exclude::add(&c, &e.core.thumbs_dir, "/").is_err(), "excluding the whole disk is refused");
    // by bare name
    let removed = exclude::add(&c, &e.core.thumbs_dir, "SubDir").unwrap();
    assert_eq!(removed, 1, "existing items inside are dropped from the catalog");
    assert!(!rows(&e).iter().any(|r| r.path.contains("/subdir/")));
    scan_all(&e);
    assert!(!rows(&e).iter().any(|r| r.path.contains("/subdir/")), "rescans never bring it back while excluded");
    assert!(e.root.join("subdir/plain.png").exists(), "files on disk are untouched");
    // by absolute path
    exclude::remove(&c, "SubDir").unwrap();
    scan_all(&e);
    assert!(rows(&e).iter().any(|r| r.path.contains("/subdir/")), "removing the rule restores the folder on next scan");
    let abs = e.root.join("subdir").to_string_lossy().to_string();
    exclude::add(&c, &e.core.thumbs_dir, &format!("{abs}/")).unwrap();
    assert_eq!(exclude::list(&c).unwrap(), vec![abs.clone()]);
    scan_all(&e);
    assert!(!rows(&e).iter().any(|r| r.path.contains("/subdir/")));
    // a sibling folder with a similar prefix is not caught by the path rule
    fs::create_dir_all(e.root.join("subdir2")).unwrap();
    fs::copy(fixtures().join("plain.png"), e.root.join("subdir2/p.png")).unwrap();
    scan_all(&e);
    assert!(rows(&e).iter().any(|r| r.path.contains("/subdir2/")));
}

fn http_get(port: u16, path: &str, range: Option<&str>) -> (String, Vec<u8>) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let r = range.map(|r| format!("Range: {r}\r\n")).unwrap_or_default();
    write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n{r}\r\n").unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let i = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (String::from_utf8_lossy(&buf[..i]).to_string(), buf[i + 4..].to_vec())
}

#[test]
fn media_server_streams_ranges_and_refuses_everything_else() {
    let e = setup();
    scan_all(&e);
    let id = rows(&e).into_iter().find(|r| r.kind == "video").unwrap().id;
    let st = stream::start(e.core.db.clone(), e.core.previews_dir.clone()).unwrap();
    let url = st.media_url(id);
    let path = url.split(&format!(":{}", st.port)).nth(1).unwrap().to_string();
    let orig = fs::read(e.root.join("clip.mp4")).unwrap();

    let (h, body) = http_get(st.port, &path, None);
    assert!(h.starts_with("HTTP/1.1 200"), "{h}");
    assert!(h.contains("Content-Type: video/mp4") && h.contains("Accept-Ranges: bytes"));
    assert_eq!(body, orig);

    let (h, body) = http_get(st.port, &path, Some("bytes=10-49"));
    assert!(h.starts_with("HTTP/1.1 206"), "{h}");
    assert!(h.contains(&format!("Content-Range: bytes 10-49/{}", orig.len())));
    assert_eq!(body, &orig[10..50]);

    let (h, _) = http_get(st.port, &path, Some("bytes=99999999-"));
    assert!(h.starts_with("HTTP/1.1 416"), "{h}");

    // wrong token, images, unknown ids and traversal are all refused
    assert!(http_get(st.port, &path.replace(&st.token, "deadbeef"), None).0.starts_with("HTTP/1.1 404"));
    let img = rows(&e).into_iter().find(|r| r.kind == "image").unwrap().id;
    assert!(http_get(st.port, &format!("/{}/m/{}", st.token, img), None).0.starts_with("HTTP/1.1 404"));
    assert!(http_get(st.port, &format!("/{}/m/999999", st.token), None).0.starts_with("HTTP/1.1 404"));
    assert!(http_get(st.port, &format!("/{}/p/..%2F..%2Fetc%2Fpasswd", st.token), None).0.starts_with("HTTP/1.1 404"));
    assert!(http_get(st.port, &format!("/{}/p/../library.sqlite3", st.token), None).0.starts_with("HTTP/1.1 404"));
}

#[test]
fn playback_copy_is_made_in_cache_and_original_is_untouched() {
    let e = setup();
    let src = e.root.join("clip.mp4");
    let before = fs::read(&src).unwrap();
    let out = e.core.previews_dir.join("playback-test.webm");
    assert!(media::transcode_for_playback(&src, &out, &AtomicBool::new(false)));
    assert!(fs::metadata(&out).unwrap().len() > 100);
    let probe = media::probe(&out).expect("probe converted copy");
    assert!(probe.duration.unwrap_or(0.0) > 0.1, "converted copy has a duration");
    assert_eq!(fs::read(&src).unwrap(), before, "original is byte-identical");
    assert!(!e.core.previews_dir.join("playback-test.part.webm").exists(), "no partial file left behind");
    // cancelling leaves nothing behind
    let out2 = e.core.previews_dir.join("playback-cancel.webm");
    assert!(!media::transcode_for_playback(&src, &out2, &AtomicBool::new(true)));
    assert!(!out2.exists());
}


#[test]
fn appimage_env_is_cleaned_for_host_tools() {
    let app = "/tmp/appimage_extracted_x";
    let ld = format!("{app}/usr/lib/:{app}/usr/lib/x86_64-linux-gnu/:{app}/lib/:{app}");
    assert_eq!(media::host_env_value("LD_LIBRARY_PATH", &ld, app), Some(None), "all bundle entries -> unset");
    assert_eq!(media::host_env_value("LD_LIBRARY_PATH", &format!("{ld}:/opt/real/lib"), app), Some(Some("/opt/real/lib".to_string())), "host entries are kept");
    assert_eq!(media::host_env_value("PATH", &format!("{app}/usr/bin/:/usr/bin:/bin"), app), Some(Some("/usr/bin:/bin".to_string())));
    assert_eq!(media::host_env_value("PATH", "/usr/bin:/bin", app), None, "untouched when nothing points into the bundle");
    assert_eq!(media::host_env_value("GST_PLUGIN_SYSTEM_PATH_1_0", &format!("{app}/usr/lib/gstreamer-1.0:"), app), Some(None));
    assert_eq!(media::host_env_value("GST_PLUGIN_SYSTEM_PATH_1_0", "/usr/lib/gstreamer-1.0", app), None);
    assert_eq!(media::host_env_value("PYTHONHOME", &format!("{app}/usr/"), app), Some(None));
    assert_eq!(media::host_env_value("HOME", "/home/u", app), None);
    assert_eq!(media::host_env_value("LD_LIBRARY_PATH", &ld, ""), None, "not an AppImage: never touch the environment");
    // a sibling directory that merely shares the prefix is not inside the bundle
    assert_eq!(media::host_env_value("PATH", "/tmp/appimage_extracted_xyz/bin:/usr/bin", app), None);
}

#[test]
fn legacy_data_folder_is_migrated_even_if_webengine_created_the_new_one() {
    let t = TempDir::new().unwrap();
    let (old, new) = (t.path().join("io.luma.photomanager"), t.path().join("io.gallerydesk.app"));
    fs::create_dir_all(old.join("thumbnails")).unwrap();
    fs::write(old.join("library.sqlite3"), b"catalog").unwrap();
    fs::write(old.join("thumbnails/a.jpg"), b"jpg").unwrap();
    // the web engine had already created the new folder with its own content, plus an empty previews/
    fs::create_dir_all(new.join("WebKitCache")).unwrap();
    fs::create_dir_all(new.join("previews")).unwrap();
    gallerydesk_lib::migrate_legacy_dir(&old, &new);
    assert_eq!(fs::read(new.join("library.sqlite3")).unwrap(), b"catalog");
    assert_eq!(fs::read(new.join("thumbnails/a.jpg")).unwrap(), b"jpg");
    assert!(new.join("WebKitCache").is_dir());
    // running again (or with an existing catalog) never overwrites anything
    fs::write(new.join("library.sqlite3"), b"newer").unwrap();
    fs::write(old.join("library.sqlite3"), b"catalog").unwrap();
    gallerydesk_lib::migrate_legacy_dir(&old, &new);
    assert_eq!(fs::read(new.join("library.sqlite3")).unwrap(), b"newer");
}

#[test]
fn upgrade_retries_failed_previews_and_reenables_video_player() {
    let t = TempDir::new().unwrap();
    let c = Core::new(t.path().to_path_buf()).unwrap();
    let conn = c.db.open().unwrap();
    conn.execute("INSERT INTO folders(path,added) VALUES('/x',1)", []).unwrap();
    conn.execute("INSERT INTO media(folder_id,path,dir,name,kind,bytes,modified,added,sort_date,seen,meta_state) VALUES(1,'/x/a.jpg','/x','a.jpg','image',1,1,1,1,1,2)", []).unwrap();
    for (k, v) in [("video_builtin", "0"), ("video_guard", "1"), ("app_version", "0.0.1")] {
        conn.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [k, v]).unwrap();
    }
    drop(conn); drop(c);
    let c = Core::new(t.path().to_path_buf()).unwrap(); // "new version starts"
    let conn = c.db.open().unwrap();
    let st: i64 = conn.query_row("SELECT meta_state FROM media", [], |r| r.get(0)).unwrap();
    assert_eq!(st, 0, "failed previews are queued again after an upgrade");
    assert_eq!(gallerydesk_lib::db::setting(&conn, "video_builtin").as_deref(), Some("1"));
    assert_eq!(gallerydesk_lib::db::setting(&conn, "video_guard").as_deref(), Some("0"));
    // a second start of the same version must not touch anything again
    conn.execute("UPDATE media SET meta_state=2", []).unwrap(); drop(conn); drop(c);
    let c = Core::new(t.path().to_path_buf()).unwrap();
    let st: i64 = c.db.open().unwrap().query_row("SELECT meta_state FROM media", [], |r| r.get(0)).unwrap();
    assert_eq!(st, 2);
}
