//! SQLite catalog: connection handling, schema and migrations.
//!
//! Every caller opens its own short-lived connection (WAL mode, busy timeout) instead of
//! sharing one connection behind a global mutex. That is what lets scanning, thumbnail
//! workers and UI queries run concurrently without the UI waiting on a long scan.

use rusqlite::Connection;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone)]
pub struct Db {
    pub path: PathBuf,
}

impl Db {
    pub fn open(&self) -> rusqlite::Result<Connection> {
        let c = Connection::open(&self.path)?;
        c.busy_timeout(Duration::from_secs(10))?;
        c.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
        Ok(c)
    }
}

const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS folders (
  id        INTEGER PRIMARY KEY,
  path      TEXT NOT NULL UNIQUE,
  added     INTEGER NOT NULL,
  last_scan INTEGER
);
CREATE TABLE IF NOT EXISTS media (
  id         INTEGER PRIMARY KEY,
  folder_id  INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
  path       TEXT NOT NULL UNIQUE,
  dir        TEXT NOT NULL,
  name       TEXT NOT NULL,
  kind       TEXT NOT NULL,
  bytes      INTEGER NOT NULL DEFAULT 0,
  modified   INTEGER NOT NULL DEFAULT 0,
  added      INTEGER NOT NULL DEFAULT 0,
  taken      INTEGER,
  sort_date  INTEGER NOT NULL DEFAULT 0,
  width      INTEGER,
  height     INTEGER,
  duration   REAL,
  camera     TEXT,
  lens       TEXT,
  iso        INTEGER,
  aperture   REAL,
  exposure   TEXT,
  focal      REAL,
  lat        REAL,
  lon        REAL,
  favorite   INTEGER NOT NULL DEFAULT 0,
  rotation   INTEGER NOT NULL DEFAULT 0,
  thumb      TEXT,
  meta_state INTEGER NOT NULL DEFAULT 0,
  hash       TEXT,
  seen       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_media_folder ON media(folder_id);
CREATE INDEX IF NOT EXISTS idx_media_date   ON media(sort_date);
CREATE INDEX IF NOT EXISTS idx_media_name   ON media(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_media_kind   ON media(kind);
CREATE INDEX IF NOT EXISTS idx_media_bytes  ON media(bytes);
CREATE INDEX IF NOT EXISTS idx_media_dir    ON media(dir);
CREATE INDEX IF NOT EXISTS idx_media_state  ON media(meta_state);
CREATE INDEX IF NOT EXISTS idx_media_fav    ON media(favorite) WHERE favorite = 1;
CREATE TABLE IF NOT EXISTS albums (
  id      INTEGER PRIMARY KEY,
  name    TEXT NOT NULL UNIQUE COLLATE NOCASE,
  created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS album_items (
  album_id INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
  media_id INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
  added    INTEGER NOT NULL,
  PRIMARY KEY (album_id, media_id)
);
CREATE INDEX IF NOT EXISTS idx_album_items_media ON album_items(media_id);
CREATE TABLE IF NOT EXISTS tags (
  id   INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE COLLATE NOCASE
);
CREATE TABLE IF NOT EXISTS media_tags (
  media_id INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
  tag_id   INTEGER NOT NULL REFERENCES tags(id)  ON DELETE CASCADE,
  PRIMARY KEY (media_id, tag_id)
);
CREATE INDEX IF NOT EXISTS idx_media_tags_tag ON media_tags(tag_id);
CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
";

fn has_column(c: &Connection, table: &str, col: &str) -> bool {
    let mut stmt = match c.prepare(&format!("PRAGMA table_info({table})")) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let names = stmt.query_map([], |r| r.get::<_, String>(1));
    match names {
        Ok(it) => it.filter_map(Result::ok).any(|n| n == col),
        Err(_) => false,
    }
}

fn table_exists(c: &Connection, table: &str) -> bool {
    c.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// Create or migrate the catalog. Returns an error string suitable for showing the user.
pub fn init(db: &Db, thumbs_dir: &Path) -> Result<(), String> {
    let c = db.open().map_err(|e| format!("could not open catalog: {e}"))?;
    c.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))
        .map_err(|e| e.to_string())?;

    // The 0.1.0 starter used a different, path-keyed `media` table with no folder roots.
    // It held nothing a user created (no favorites/albums/tags), so it is safe to drop and
    // rebuild from the originals. Its path-hash thumbnails are orphaned, so purge them.
    if table_exists(&c, "media") && !has_column(&c, "media", "folder_id") {
        c.execute_batch("DROP TABLE media;").map_err(|e| e.to_string())?;
        if let Ok(rd) = fs::read_dir(thumbs_dir) {
            for e in rd.flatten() {
                if e.path().extension().and_then(|x| x.to_str()) == Some("jpg") {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
    }

    let version: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if version < 1 {
        c.execute_batch(SCHEMA_V1).map_err(|e| e.to_string())?;
        c.execute_batch("PRAGMA user_version=1;").map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn setting(c: &Connection, key: &str) -> Option<String> {
    c.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0)).ok()
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A restore is staged as `library.restore.pending` and swapped in here, before the
/// catalog is opened, so we never replace a database that is in use.
pub fn apply_pending_restore(data_dir: &Path, db_path: &Path) {
    let pending = data_dir.join("library.restore.pending");
    if pending.exists() {
        let _ = fs::remove_file(db_path.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(db_path.with_extension("sqlite3-shm"));
        if fs::rename(&pending, db_path).is_err() {
            if fs::copy(&pending, db_path).is_ok() {
                let _ = fs::remove_file(&pending);
            }
        }
    }
}
