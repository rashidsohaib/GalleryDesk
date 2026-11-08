//! Loopback-only media streaming for the in-app video player.
//!
//! WebKitGTK's media pipeline (GStreamer) cannot stream from Tauri's custom `asset:` scheme, so
//! videos would never play there. Instead a tiny HTTP/1.1 server listens on 127.0.0.1 (never on a
//! network interface), requires a random per-launch token in every URL, only serves files that are
//! in the catalog (or the converted-playback cache), and supports `Range` so seeking works.
//! Nothing here leaves the computer and files are only ever read.

use crate::db::Db;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::PathBuf,
    thread,
    time::Duration,
};

pub struct Stream { pub port: u16, pub token: String }

impl Stream {
    pub fn media_url(&self, id: i64) -> String { format!("http://127.0.0.1:{}/{}/m/{}", self.port, self.token, id) }
    pub fn preview_url(&self, file: &str) -> String { format!("http://127.0.0.1:{}/{}/p/{}", self.port, self.token, file) }
}

fn random_token() -> String {
    let mut b = [0u8; 16];
    let ok = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_ok();
    if !ok {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(1);
        let mix = n ^ ((std::process::id() as u128) << 64) ^ (&b as *const _ as usize as u128).rotate_left(17);
        b = mix.to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn start(db: Db, previews: PathBuf) -> std::io::Result<Stream> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let token = random_token();
    let tok = token.clone();
    thread::Builder::new().name("media-stream".into()).spawn(move || {
        for conn in listener.incoming().flatten() {
            let (db, previews, tok) = (db.clone(), previews.clone(), tok.clone());
            let _ = thread::Builder::new().name("media-conn".into()).spawn(move || { let _ = handle(conn, &db, &previews, &tok); });
        }
    })?;
    Ok(Stream { port, token })
}

fn mime(p: &std::path::Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("mov") => "video/quicktime",
        Some("avi") => "video/x-msvideo",
        Some("ogv") => "video/ogg",
        Some("mpg") | Some("mpeg") => "video/mpeg",
        Some("3gp") => "video/3gpp",
        _ => "application/octet-stream",
    }
}

fn reply(s: &mut TcpStream, code: &str, extra: &str) {
    let _ = write!(s, "HTTP/1.1 {code}\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n");
}

/// Parse a single `bytes=` range against a file of `len` bytes. None = unsatisfiable.
fn parse_range(h: &str, len: u64) -> Option<(u64, u64)> {
    let spec = h.trim().strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    if len == 0 { return None; }
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 { return None; }
        (len.saturating_sub(n), len - 1)
    } else {
        let s: u64 = a.parse().ok()?;
        let e: u64 = if b.is_empty() { len - 1 } else { b.parse::<u64>().ok()?.min(len - 1) };
        (s, e)
    };
    if start > end || start >= len { None } else { Some((start, end)) }
}

fn handle(mut s: TcpStream, db: &Db, previews: &PathBuf, token: &str) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(15)))?;
    s.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut rd = BufReader::new(s.try_clone()?);
    let mut line = String::new();
    rd.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let mut range: Option<String> = None;
    for _ in 0..64 {
        let mut h = String::new();
        if rd.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" { break; }
        if let Some((k, v)) = h.split_once(':') { if k.eq_ignore_ascii_case("range") { range = Some(v.trim().to_string()); } }
    }
    if method != "GET" && method != "HEAD" { reply(&mut s, "405 Method Not Allowed", "Allow: GET, HEAD\r\n"); return Ok(()); }

    let path = target.split('?').next().unwrap_or("");
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if seg.len() != 3 || seg[0] != token { reply(&mut s, "404 Not Found", ""); return Ok(()); }
    let file: PathBuf = match seg[1] {
        "m" => {
            let Ok(id) = seg[2].parse::<i64>() else { reply(&mut s, "404 Not Found", ""); return Ok(()); };
            let p: Option<String> = db.open().ok().and_then(|c| c.query_row("SELECT path FROM media WHERE id=?1 AND kind='video'", [id], |r| r.get(0)).ok());
            match p { Some(p) => PathBuf::from(p), None => { reply(&mut s, "404 Not Found", ""); return Ok(()); } }
        }
        "p" => {
            let name = seg[2];
            let ok = name.ends_with(".webm") && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_hexdigit() || b == b'.' || b == b'w' || b == b'e' || b == b'b' || b == b'm');
            if !ok { reply(&mut s, "404 Not Found", ""); return Ok(()); }
            previews.join(name)
        }
        _ => { reply(&mut s, "404 Not Found", ""); return Ok(()); }
    };
    let mut f = match File::open(&file) { Ok(f) => f, Err(_) => { reply(&mut s, "404 Not Found", ""); return Ok(()); } };
    let len = f.metadata()?.len();
    let ct = mime(&file);

    let (status, start, end, extra) = match range.as_deref() {
        Some(r) => match parse_range(r, len) {
            Some((a, b)) => ("206 Partial Content", a, b, format!("Content-Range: bytes {a}-{b}/{len}\r\n")),
            None => { reply(&mut s, "416 Range Not Satisfiable", &format!("Content-Range: bytes */{len}\r\n")); return Ok(()); }
        },
        None => ("200 OK", 0, len.saturating_sub(1), String::new()),
    };
    let n = if len == 0 { 0 } else { end - start + 1 };
    write!(s, "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nAccept-Ranges: bytes\r\n{extra}Content-Length: {n}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")?;
    if method == "HEAD" || n == 0 { return Ok(()); }
    f.seek(SeekFrom::Start(start))?;
    let mut left = n;
    let mut buf = vec![0u8; 256 * 1024];
    while left > 0 {
        let want = buf.len().min(left as usize);
        let got = f.read(&mut buf[..want])?;
        if got == 0 { break; }
        if s.write_all(&buf[..got]).is_err() { break; } // player seeked away or closed
        left -= got as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_range;
    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=0-5000", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-2", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
    }
}
