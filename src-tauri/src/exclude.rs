//! "Exclude folders": paths (and bare folder names) the scanner must never descend into.
//!
//! A rule is either an absolute folder path (matches that folder and everything below it) or a bare
//! name without a slash, such as `node_modules` (matches any folder with that name, at any depth).
//! Rules only affect what is *indexed*; files on disk are never touched.

use rusqlite::{params, Connection};
use std::{fs, path::{Path, PathBuf}};

type R<T> = Result<T, String>;
fn e2s<E: std::fmt::Display>(e: E) -> String { e.to_string() }

#[derive(Default, Clone, Debug)]
pub struct Rules {
    paths: Vec<PathBuf>,
    names: Vec<String>,
}

impl Rules {
    pub fn from_entries<I: IntoIterator<Item = String>>(entries: I) -> Self {
        let mut r = Rules::default();
        for e in entries {
            if e.contains('/') { r.paths.push(PathBuf::from(e)); } else { r.names.push(e.to_lowercase()); }
        }
        r
    }

    pub fn is_empty(&self) -> bool { self.paths.is_empty() && self.names.is_empty() }

    /// True when this directory (given by its full path) is excluded.
    pub fn excludes_dir(&self, dir: &Path) -> bool {
        if self.paths.iter().any(|p| dir.starts_with(p)) { return true; }
        if self.names.is_empty() { return false; }
        dir.file_name().and_then(|n| n.to_str()).map(|n| self.names.contains(&n.to_lowercase())).unwrap_or(false)
    }

    /// True when a file lives inside an excluded folder (used when purging the catalog).
    pub fn excludes_file(&self, file: &Path) -> bool {
        file.ancestors().skip(1).any(|a| self.excludes_dir(a))
    }
}

pub fn ensure_table(c: &Connection) -> R<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS excluded (entry TEXT PRIMARY KEY, added INTEGER NOT NULL DEFAULT 0);").map_err(e2s)
}

pub fn list(c: &Connection) -> R<Vec<String>> {
    ensure_table(c)?;
    let mut s = c.prepare("SELECT entry FROM excluded ORDER BY entry COLLATE NOCASE").map_err(e2s)?;
    let rows = s.query_map([], |r| r.get::<_, String>(0)).map_err(e2s)?;
    rows.collect::<Result<_, _>>().map_err(e2s)
}

pub fn load(c: &Connection) -> Rules { Rules::from_entries(list(c).unwrap_or_default()) }

fn normalise(entry: &str) -> R<String> {
    let t = entry.trim();
    if t.is_empty() { return Err("Enter a folder path or folder name".into()); }
    if t.contains('/') {
        let p = Path::new(t);
        if !p.is_absolute() { return Err("Use a full path such as /home/me/Pictures/Screenshots, or just a folder name".into()); }
        let trimmed = t.trim_end_matches('/');
        Ok(if trimmed.is_empty() { "/".into() } else { trimmed.to_string() })
    } else {
        if t == "." || t == ".." { return Err("That is not a folder name".into()); }
        Ok(t.to_string())
    }
}

/// Add a rule and drop anything it covers from the catalog. Returns how many items were removed.
pub fn add(c: &Connection, thumbs: &Path, entry: &str) -> R<usize> {
    ensure_table(c)?;
    let entry = normalise(entry)?;
    if entry == "/" { return Err("Excluding the whole disk would hide everything".into()); }
    c.execute("INSERT OR IGNORE INTO excluded(entry, added) VALUES(?1, strftime('%s','now'))", params![entry]).map_err(e2s)?;
    purge(c, thumbs)
}

pub fn remove(c: &Connection, entry: &str) -> R<()> {
    ensure_table(c)?;
    c.execute("DELETE FROM excluded WHERE entry=?1", params![entry]).map_err(e2s)?;
    Ok(())
}

/// Delete catalog rows (and their cached thumbnails) that sit inside an excluded folder.
pub fn purge(c: &Connection, thumbs: &Path) -> R<usize> {
    let rules = load(c);
    if rules.is_empty() { return Ok(0); }
    let hits: Vec<(i64, Option<String>)> = {
        let mut s = c.prepare("SELECT id, path, thumb FROM media").map_err(e2s)?;
        let rows = s.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))).map_err(e2s)?;
        let mut v = Vec::new();
        for row in rows { let (id, p, t) = row.map_err(e2s)?; if rules.excludes_file(Path::new(&p)) { v.push((id, t)); } }
        v
    };
    c.execute_batch("BEGIN").map_err(e2s)?;
    for (id, _) in &hits { c.execute("DELETE FROM media WHERE id=?1", [id]).map_err(e2s)?; }
    c.execute_batch("COMMIT").map_err(e2s)?;
    for (_, t) in &hits { if let Some(n) = t { let _ = fs::remove_file(thumbs.join(n)); } }
    Ok(hits.len())
}
