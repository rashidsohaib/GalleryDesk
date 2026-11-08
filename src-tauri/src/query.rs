//! Read/write operations on the catalog used by the UI. All functions take a plain
//! `rusqlite::Connection` so they can be tested without a Tauri runtime.

use crate::db::now_secs;
use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

type R<T> = Result<T, String>;
fn e2s(e: rusqlite::Error) -> String { e.to_string() }

// ------------------------------------------------------------------ filters

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Filter {
    pub search: String,
    /// "all" | "image" | "video"
    pub kind: String,
    /// "all" | "favorites" | "folder" | "album" | "tag" | "smart"
    pub scope: String,
    pub folder_id: Option<i64>,
    pub dir: Option<String>,
    pub album_id: Option<i64>,
    pub tag: Option<String>,
    /// "recent" | "untagged" | "gps"
    pub smart: Option<String>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Sort {
    /// "date" | "name" | "added" | "size" | "kind"
    pub key: String,
    pub desc: bool,
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn build_where(f: &Filter) -> (String, Vec<Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut p: Vec<Value> = Vec::new();

    let search = f.search.trim();
    if !search.is_empty() {
        let pat = format!("%{}%", like_escape(search));
        clauses.push(
            "(m.name LIKE ? ESCAPE '\\' OR m.camera LIKE ? ESCAPE '\\' OR EXISTS (SELECT 1 FROM media_tags mt JOIN tags t ON t.id=mt.tag_id WHERE mt.media_id=m.id AND t.name LIKE ? ESCAPE '\\'))".into(),
        );
        for _ in 0..3 { p.push(Value::Text(pat.clone())); }
    }
    match f.kind.as_str() {
        "image" | "video" => { clauses.push("m.kind=?".into()); p.push(Value::Text(f.kind.clone())); }
        _ => {}
    }
    match f.scope.as_str() {
        "favorites" => clauses.push("m.favorite=1".into()),
        "folder" => {
            if let Some(id) = f.folder_id { clauses.push("m.folder_id=?".into()); p.push(Value::Integer(id)); }
            if let Some(dir) = &f.dir {
                clauses.push("(m.dir=? OR m.dir LIKE ? ESCAPE '\\')".into());
                p.push(Value::Text(dir.clone()));
                p.push(Value::Text(format!("{}/%", like_escape(dir.trim_end_matches('/')))));
            }
        }
        "album" => {
            clauses.push("EXISTS (SELECT 1 FROM album_items ai WHERE ai.media_id=m.id AND ai.album_id=?)".into());
            p.push(Value::Integer(f.album_id.unwrap_or(-1)));
        }
        "tag" => {
            clauses.push("EXISTS (SELECT 1 FROM media_tags mt JOIN tags t ON t.id=mt.tag_id WHERE mt.media_id=m.id AND t.name=? COLLATE NOCASE)".into());
            p.push(Value::Text(f.tag.clone().unwrap_or_default()));
        }
        "smart" => match f.smart.as_deref() {
            Some("recent") => { clauses.push("m.added>=?".into()); p.push(Value::Integer(now_secs() - 30 * 86400)); }
            Some("untagged") => clauses.push("NOT EXISTS (SELECT 1 FROM media_tags mt WHERE mt.media_id=m.id)".into()),
            Some("gps") => clauses.push("m.lat IS NOT NULL AND m.lon IS NOT NULL".into()),
            _ => {}
        },
        _ => {}
    }
    let sql = if clauses.is_empty() { String::new() } else { format!("WHERE {}", clauses.join(" AND ")) };
    (sql, p)
}

fn order_by(s: &Sort) -> String {
    let dir = if s.desc { "DESC" } else { "ASC" };
    let col = match s.key.as_str() {
        "name" => "m.name COLLATE NOCASE",
        "added" => "m.added",
        "size" => "m.bytes",
        "kind" => "m.kind",
        _ => "m.sort_date",
    };
    format!("ORDER BY {col} {dir}, m.id {dir}")
}

// ------------------------------------------------------------------ items

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MediaItem {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub bytes: i64,
    pub modified: i64,
    pub date: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration: Option<f64>,
    pub favorite: bool,
    pub rotation: i64,
    pub thumbnail: Option<String>,
    /// 0 = queued, 1 = ready, 2 = could not be decoded
    pub state: i64,
}

const ITEM_COLS: &str = "m.id,m.path,m.name,m.kind,m.bytes,m.modified,m.sort_date,m.width,m.height,m.duration,m.favorite,m.rotation,m.thumb,m.meta_state";

fn item_from_row(r: &rusqlite::Row, thumbs: &Path) -> rusqlite::Result<MediaItem> {
    let thumb: Option<String> = r.get(12)?;
    Ok(MediaItem {
        id: r.get(0)?, path: r.get(1)?, name: r.get(2)?, kind: r.get(3)?, bytes: r.get(4)?,
        modified: r.get(5)?, date: r.get(6)?, width: r.get(7)?, height: r.get(8)?, duration: r.get(9)?,
        favorite: r.get::<_, i64>(10)? != 0, rotation: r.get(11)?,
        thumbnail: thumb.map(|t| thumbs.join(t).to_string_lossy().to_string()),
        state: r.get(13)?,
    })
}

#[derive(Serialize, Debug)]
pub struct Group { pub key: String, pub count: i64 }

#[derive(Serialize, Debug)]
pub struct Layout { pub total: i64, pub groups: Vec<Group> }

/// `desc` must match the item sort direction so group headers line up with the rows below them.
pub fn query_layout(c: &Connection, f: &Filter, group: &str, desc: bool) -> R<Layout> {
    let (w, p) = build_where(f);
    let total: i64 = c
        .query_row(&format!("SELECT COUNT(*) FROM media m {w}"), params_from_iter(p.iter()), |r| r.get(0))
        .map_err(e2s)?;
    let fmt = match group { "day" => "%Y-%m-%d", "month" => "%Y-%m", "year" => "%Y", _ => "" };
    let mut groups = Vec::new();
    if !fmt.is_empty() && total > 0 {
        let sql = format!("SELECT strftime('{fmt}', m.sort_date, 'unixepoch') k, COUNT(*) FROM media m {w} GROUP BY k ORDER BY k {}", if desc { "DESC" } else { "ASC" });
        let mut s = c.prepare(&sql).map_err(e2s)?;
        let rows = s.query_map(params_from_iter(p.iter()), |r| Ok(Group { key: r.get(0)?, count: r.get(1)? })).map_err(e2s)?;
        groups = rows.collect::<Result<Vec<_>, _>>().map_err(e2s)?;
    }
    Ok(Layout { total, groups })
}

pub fn query_media(c: &Connection, thumbs: &Path, f: &Filter, s: &Sort, offset: i64, limit: i64) -> R<Vec<MediaItem>> {
    let (w, mut p) = build_where(f);
    p.push(Value::Integer(limit.clamp(1, 2000)));
    p.push(Value::Integer(offset.max(0)));
    let sql = format!("SELECT {ITEM_COLS} FROM media m {w} {} LIMIT ? OFFSET ?", order_by(s));
    let mut stmt = c.prepare(&sql).map_err(e2s)?;
    let rows = stmt.query_map(params_from_iter(p.iter()), |r| item_from_row(r, thumbs)).map_err(e2s)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(e2s)
}

pub fn query_ids(c: &Connection, f: &Filter, s: &Sort, offset: i64, limit: i64) -> R<Vec<i64>> {
    let (w, mut p) = build_where(f);
    p.push(Value::Integer(limit.clamp(1, 200_000)));
    p.push(Value::Integer(offset.max(0)));
    let sql = format!("SELECT m.id FROM media m {w} {} LIMIT ? OFFSET ?", order_by(s));
    let mut stmt = c.prepare(&sql).map_err(e2s)?;
    let rows = stmt.query_map(params_from_iter(p.iter()), |r| r.get(0)).map_err(e2s)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(e2s)
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AlbumRef { pub id: i64, pub name: String }

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Detail {
    #[serde(flatten)]
    pub item: MediaItem,
    pub dir: String,
    pub added: i64,
    pub taken: Option<i64>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<i64>,
    pub aperture: Option<f64>,
    pub exposure: Option<String>,
    pub focal: Option<f64>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub tags: Vec<String>,
    pub albums: Vec<AlbumRef>,
}

pub fn get_media(c: &Connection, thumbs: &Path, id: i64) -> R<Detail> {
    let sql = format!("SELECT {ITEM_COLS}, m.dir,m.added,m.taken,m.camera,m.lens,m.iso,m.aperture,m.exposure,m.focal,m.lat,m.lon FROM media m WHERE m.id=?1");
    let mut d = c
        .query_row(&sql, [id], |r| {
            Ok(Detail {
                item: item_from_row(r, thumbs)?,
                dir: r.get(14)?, added: r.get(15)?, taken: r.get(16)?, camera: r.get(17)?, lens: r.get(18)?,
                iso: r.get(19)?, aperture: r.get(20)?, exposure: r.get(21)?, focal: r.get(22)?,
                lat: r.get(23)?, lon: r.get(24)?, tags: vec![], albums: vec![],
            })
        })
        .map_err(|_| "That item is no longer in the library".to_string())?;
    let mut s = c.prepare("SELECT t.name FROM media_tags mt JOIN tags t ON t.id=mt.tag_id WHERE mt.media_id=?1 ORDER BY t.name COLLATE NOCASE").map_err(e2s)?;
    d.tags = s.query_map([id], |r| r.get(0)).map_err(e2s)?.collect::<Result<_, _>>().map_err(e2s)?;
    let mut s = c.prepare("SELECT a.id,a.name FROM album_items ai JOIN albums a ON a.id=ai.album_id WHERE ai.media_id=?1 ORDER BY a.name COLLATE NOCASE").map_err(e2s)?;
    d.albums = s.query_map([id], |r| Ok(AlbumRef { id: r.get(0)?, name: r.get(1)? })).map_err(e2s)?.collect::<Result<_, _>>().map_err(e2s)?;
    Ok(d)
}

// ------------------------------------------------------------------ folders & stats

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FolderInfo { pub id: i64, pub path: String, pub name: String, pub count: i64, pub available: bool, pub last_scan: Option<i64> }

pub fn list_folders(c: &Connection) -> R<Vec<FolderInfo>> {
    let mut s = c.prepare("SELECT f.id,f.path,f.last_scan,(SELECT COUNT(*) FROM media m WHERE m.folder_id=f.id) FROM folders f ORDER BY f.path COLLATE NOCASE").map_err(e2s)?;
    let rows = s.query_map([], |r| {
        let path: String = r.get(1)?;
        let name = Path::new(&path).file_name().and_then(|n| n.to_str()).unwrap_or(&path).to_string();
        let available = Path::new(&path).is_dir();
        Ok(FolderInfo { id: r.get(0)?, path, name, count: r.get(3)?, available, last_scan: r.get(2)? })
    }).map_err(e2s)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(e2s)
}

#[derive(Serialize, Debug)]
pub struct DirCount { pub dir: String, pub count: i64 }

pub fn list_dirs(c: &Connection, folder_id: i64) -> R<Vec<DirCount>> {
    let mut s = c.prepare("SELECT dir, COUNT(*) FROM media WHERE folder_id=?1 GROUP BY dir ORDER BY dir COLLATE NOCASE").map_err(e2s)?;
    let rows = s.query_map([folder_id], |r| Ok(DirCount { dir: r.get(0)?, count: r.get(1)? })).map_err(e2s)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(e2s)
}

pub fn add_folder(c: &Connection, path: &str) -> R<i64> {
    let p = PathBuf::from(path);
    if !p.is_dir() { return Err("That path is not a folder".into()); }
    let canon = fs::canonicalize(&p).unwrap_or(p).to_string_lossy().to_string();
    // Reject folders nested in (or containing) an existing root: they would double-index files.
    let existing: Vec<(i64, String)> = {
        let mut s = c.prepare("SELECT id,path FROM folders").map_err(e2s)?;
        let rows = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(e2s)?;
        rows.collect::<Result<_, _>>().map_err(e2s)?
    };
    for (id, ex) in &existing {
        if *ex == canon { return Ok(*id); }
        if Path::new(&canon).starts_with(ex) { return Err(format!("Already covered by \"{ex}\"")); }
        if Path::new(ex).starts_with(&canon) { return Err(format!("This folder contains \"{ex}\", which is already in the library. Remove that one first.")); }
    }
    c.execute("INSERT INTO folders(path,added) VALUES(?1,?2)", params![canon, now_secs()]).map_err(e2s)?;
    Ok(c.last_insert_rowid())
}

/// Removes a folder from the catalog and deletes its cached thumbnails. Never touches originals.
pub fn remove_folder(c: &Connection, thumbs: &Path, id: i64) -> R<()> {
    let names: Vec<String> = {
        let mut s = c.prepare("SELECT thumb FROM media WHERE folder_id=?1 AND thumb IS NOT NULL").map_err(e2s)?;
        let rows = s.query_map([id], |r| r.get(0)).map_err(e2s)?;
        rows.collect::<Result<_, _>>().map_err(e2s)?
    };
    c.execute("DELETE FROM folders WHERE id=?1", [id]).map_err(e2s)?;
    for n in names { let _ = fs::remove_file(thumbs.join(n)); }
    Ok(())
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Stats { pub total: i64, pub images: i64, pub videos: i64, pub favorites: i64, pub bytes: i64, pub pending: i64, pub failed: i64 }

pub fn library_stats(c: &Connection) -> R<Stats> {
    c.query_row(
        "SELECT COUNT(*), COALESCE(SUM(kind='image'),0), COALESCE(SUM(kind='video'),0), COALESCE(SUM(favorite),0),
                COALESCE(SUM(bytes),0), COALESCE(SUM(meta_state=0),0), COALESCE(SUM(meta_state=2),0) FROM media",
        [],
        |r| Ok(Stats { total: r.get(0)?, images: r.get(1)?, videos: r.get(2)?, favorites: r.get(3)?, bytes: r.get(4)?, pending: r.get(5)?, failed: r.get(6)? }),
    ).map_err(e2s)
}

// ------------------------------------------------------------------ mutations

fn id_list(ids: &[i64]) -> String {
    ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
}

pub fn set_favorite(c: &Connection, ids: &[i64], value: bool) -> R<()> {
    if ids.is_empty() { return Ok(()); }
    c.execute(&format!("UPDATE media SET favorite=?1 WHERE id IN ({})", id_list(ids)), [value as i64]).map_err(e2s)?;
    Ok(())
}

/// Non-destructive: stores the rotation and queues the thumbnail for re-rendering.
pub fn rotate(c: &Connection, thumbs: &Path, ids: &[i64], delta: i64) -> R<()> {
    for id in ids {
        let old: Option<Option<String>> = c.query_row("SELECT thumb FROM media WHERE id=?1", [id], |r| r.get(0)).optional().map_err(e2s)?;
        if let Some(t) = old.flatten() { let _ = fs::remove_file(thumbs.join(t)); }
        c.execute("UPDATE media SET rotation=((rotation+?1)%360+360)%360, thumb=NULL, meta_state=0 WHERE id=?2", params![delta, id]).map_err(e2s)?;
    }
    Ok(())
}

#[derive(Serialize, Debug)]
pub struct NamedCount { pub id: i64, pub name: String, pub count: i64 }

pub fn list_albums(c: &Connection) -> R<Vec<NamedCount>> {
    let mut s = c.prepare("SELECT a.id,a.name,(SELECT COUNT(*) FROM album_items ai WHERE ai.album_id=a.id) FROM albums a ORDER BY a.name COLLATE NOCASE").map_err(e2s)?;
    let rows = s.query_map([], |r| Ok(NamedCount { id: r.get(0)?, name: r.get(1)?, count: r.get(2)? })).map_err(e2s)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(e2s)
}

fn clean_name(name: &str, what: &str) -> R<String> {
    let n = name.trim();
    if n.is_empty() { return Err(format!("{what} name cannot be empty")); }
    if n.chars().count() > 80 { return Err(format!("{what} name is too long")); }
    Ok(n.to_string())
}

pub fn create_album(c: &Connection, name: &str) -> R<i64> {
    let n = clean_name(name, "Album")?;
    if let Some(id) = c.query_row("SELECT id FROM albums WHERE name=?1", [&n], |r| r.get::<_, i64>(0)).optional().map_err(e2s)? {
        let _ = id;
        return Err(format!("An album named \"{n}\" already exists"));
    }
    c.execute("INSERT INTO albums(name,created) VALUES(?1,?2)", params![n, now_secs()]).map_err(e2s)?;
    Ok(c.last_insert_rowid())
}

pub fn rename_album(c: &Connection, id: i64, name: &str) -> R<()> {
    let n = clean_name(name, "Album")?;
    c.execute("UPDATE albums SET name=?1 WHERE id=?2", params![n, id])
        .map_err(|e| if e.to_string().contains("UNIQUE") { format!("An album named \"{n}\" already exists") } else { e.to_string() })?;
    Ok(())
}

pub fn delete_album(c: &Connection, id: i64) -> R<()> {
    c.execute("DELETE FROM albums WHERE id=?1", [id]).map_err(e2s)?;
    Ok(())
}

pub fn album_add(c: &Connection, album_id: i64, ids: &[i64]) -> R<i64> {
    let mut added = 0;
    let now = now_secs();
    for id in ids {
        added += c.execute("INSERT OR IGNORE INTO album_items(album_id,media_id,added) VALUES(?1,?2,?3)", params![album_id, id, now]).map_err(e2s)? as i64;
    }
    Ok(added)
}

pub fn album_remove(c: &Connection, album_id: i64, ids: &[i64]) -> R<()> {
    if ids.is_empty() { return Ok(()); }
    c.execute(&format!("DELETE FROM album_items WHERE album_id=?1 AND media_id IN ({})", id_list(ids)), [album_id]).map_err(e2s)?;
    Ok(())
}

pub fn list_tags(c: &Connection) -> R<Vec<NamedCount>> {
    let mut s = c.prepare("SELECT t.id,t.name,COUNT(mt.media_id) FROM tags t JOIN media_tags mt ON mt.tag_id=t.id GROUP BY t.id ORDER BY COUNT(mt.media_id) DESC, t.name COLLATE NOCASE").map_err(e2s)?;
    let rows = s.query_map([], |r| Ok(NamedCount { id: r.get(0)?, name: r.get(1)?, count: r.get(2)? })).map_err(e2s)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(e2s)
}

pub fn add_tag(c: &Connection, ids: &[i64], tag: &str) -> R<()> {
    let name = clean_name(tag.trim_start_matches('#'), "Tag")?;
    c.execute("INSERT OR IGNORE INTO tags(name) VALUES(?1)", [&name]).map_err(e2s)?;
    let tid: i64 = c.query_row("SELECT id FROM tags WHERE name=?1", [&name], |r| r.get(0)).map_err(e2s)?;
    for id in ids {
        c.execute("INSERT OR IGNORE INTO media_tags(media_id,tag_id) VALUES(?1,?2)", params![id, tid]).map_err(e2s)?;
    }
    Ok(())
}

pub fn remove_tag(c: &Connection, ids: &[i64], tag: &str) -> R<()> {
    if ids.is_empty() { return Ok(()); }
    c.execute(
        &format!("DELETE FROM media_tags WHERE media_id IN ({}) AND tag_id IN (SELECT id FROM tags WHERE name=?1)", id_list(ids)),
        [tag],
    ).map_err(e2s)?;
    c.execute("DELETE FROM tags WHERE id NOT IN (SELECT DISTINCT tag_id FROM media_tags)", []).map_err(e2s)?;
    Ok(())
}

// ------------------------------------------------------------------ settings

pub const SETTING_DEFAULTS: &[(&str, &str)] = &[
    ("theme", "system"), ("accent", "sage"), ("tile", "190"), ("fit", "fill"), ("captions", "1"),
    ("watch", "1"), ("scan_on_launch", "1"), ("workers", "0"), ("thumb_size", "480"),
    ("sort_key", "date"), ("sort_desc", "1"), ("video_builtin", "1"), ("video_guard", "0"),
];

pub fn get_settings(c: &Connection) -> R<BTreeMap<String, String>> {
    let mut m: BTreeMap<String, String> = SETTING_DEFAULTS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let mut s = c.prepare("SELECT key,value FROM settings").map_err(e2s)?;
    for row in s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(e2s)? {
        let (k, v) = row.map_err(e2s)?;
        if m.contains_key(&k) { m.insert(k, v); }
    }
    Ok(m)
}

pub fn set_setting(c: &Connection, key: &str, value: &str) -> R<()> {
    if !SETTING_DEFAULTS.iter().any(|(k, _)| *k == key) { return Err(format!("Unknown setting \"{key}\"")); }
    if value.len() > 64 { return Err("Setting value too long".into()); }
    c.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value]).map_err(e2s)?;
    Ok(())
}

// ------------------------------------------------------------------ duplicates

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DupGroup { pub bytes: i64, pub items: Vec<MediaItem> }

fn hash_file(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 { break; }
        h.update(&buf[..n]);
    }
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Exact (byte-identical) duplicates. Only files that share a size are ever read, and hashes
/// are cached in the catalog (and cleared when a file changes) so re-runs are fast.
pub fn find_duplicates(c: &Connection, thumbs: &Path, cancel: &AtomicBool, mut progress: impl FnMut(u64, u64)) -> R<Vec<DupGroup>> {
    let candidates: Vec<(i64, String, Option<String>)> = {
        let mut s = c.prepare("SELECT id,path,hash FROM media WHERE bytes>0 AND bytes IN (SELECT bytes FROM media WHERE bytes>0 GROUP BY bytes HAVING COUNT(*)>1)").map_err(e2s)?;
        let rows = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(e2s)?;
        rows.collect::<Result<_, _>>().map_err(e2s)?
    };
    let total = candidates.iter().filter(|c| c.2.is_none()).count() as u64;
    let mut done = 0u64;
    for (id, path, hash) in &candidates {
        if hash.is_some() { continue; }
        if cancel.load(Ordering::Relaxed) { return Err("Cancelled".into()); }
        if let Some(h) = hash_file(Path::new(path)) {
            c.execute("UPDATE media SET hash=?1 WHERE id=?2", params![h, id]).map_err(e2s)?;
        }
        done += 1;
        if done % 5 == 0 || done == total { progress(done, total); }
    }
    let mut s = c.prepare("SELECT hash, bytes FROM media WHERE hash IS NOT NULL GROUP BY hash, bytes HAVING COUNT(*)>1 ORDER BY bytes*(COUNT(*)-1) DESC LIMIT 500").map_err(e2s)?;
    let groups: Vec<(String, i64)> = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(e2s)?.collect::<Result<_, _>>().map_err(e2s)?;
    let mut out = Vec::new();
    let sql = format!("SELECT {ITEM_COLS} FROM media m WHERE m.hash=?1 ORDER BY m.sort_date, m.id");
    let mut stmt = c.prepare(&sql).map_err(e2s)?;
    for (hash, bytes) in groups {
        let items = stmt.query_map([&hash], |r| item_from_row(r, thumbs)).map_err(e2s)?.collect::<Result<Vec<_>, _>>().map_err(e2s)?;
        if items.len() > 1 { out.push(DupGroup { bytes, items }); }
    }
    Ok(out)
}

// ------------------------------------------------------------------ backup

pub fn backup_catalog(c: &Connection, dest: &Path) -> R<()> {
    if dest.exists() { fs::remove_file(dest).map_err(|e| format!("cannot overwrite {}: {e}", dest.display()))?; }
    let d = dest.to_string_lossy().replace('\'', "''");
    c.execute_batch(&format!("VACUUM INTO '{d}'")).map_err(e2s)
}

pub fn validate_backup(src: &Path) -> R<()> {
    let c = Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|_| "That file is not a GalleryDesk catalog".to_string())?;
    let ok: i64 = c
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('media','folders','albums','tags','settings')", [], |r| r.get(0))
        .map_err(|_| "That file is not a GalleryDesk catalog".to_string())?;
    if ok < 5 { return Err("That file is not a GalleryDesk catalog".into()); }
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
    if v != 1 { return Err("This catalog was made by a different version of GalleryDesk".into()); }
    Ok(())
}

/// After a restore, a cache wipe or a crash, a row can point at a thumbnail that no longer
/// exists. Queue those rows for re-rendering. Returns how many were queued.
pub fn requeue_missing_thumbs(c: &Connection, thumbs: &Path) -> R<usize> {
    let present: std::collections::HashSet<String> = fs::read_dir(thumbs)
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_str().map(String::from)).collect())
        .unwrap_or_default();
    let rows: Vec<(i64, String)> = {
        let mut s = c.prepare("SELECT id,thumb FROM media WHERE meta_state=1 AND thumb IS NOT NULL").map_err(e2s)?;
        let it = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(e2s)?;
        it.collect::<Result<_, _>>().map_err(e2s)?
    };
    let mut n = 0;
    for (id, t) in rows {
        if !present.contains(&t) {
            c.execute("UPDATE media SET thumb=NULL, meta_state=0 WHERE id=?1", [id]).map_err(e2s)?;
            n += 1;
        }
    }
    Ok(n)
}

pub fn dir_size(dir: &Path) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() { if m.is_file() { bytes += m.len(); files += 1; } }
        }
    }
    (bytes, files)
}
