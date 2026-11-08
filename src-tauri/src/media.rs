//! Media classification, metadata extraction (EXIF / ffprobe) and thumbnail rendering.
//! Nothing here ever writes next to, or into, an original file: all output goes to the
//! application cache directory.

use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

const IMAGE_EXT: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif",
    "avif", "jxl", "jp2", "j2k", "psd", "tga", "exr", "dng", "cr2", "cr3", "nef", "nrw", "arw",
    "srf", "sr2", "raf", "rw2", "orf", "pef", "srw", "rwl", "3fr", "mos", "dcr", "kdc", "erf",
    "mef", "mrw",
];
const RAW_EXT: &[&str] = &[
    "dng", "cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "raf", "rw2", "orf", "pef", "srw",
    "rwl", "3fr", "mos", "dcr", "kdc", "erf", "mef", "mrw",
];
const VIDEO_EXT: &[&str] = &[
    "mp4", "mov", "m4v", "mkv", "avi", "flv", "ts", "mts", "m2ts", "wmv", "webm", "3gp", "3g2",
    "f4v", "vob", "mpg", "mpeg", "asf", "divx", "ogv",
];

fn ext_of(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_ascii_lowercase())
}

pub fn media_kind(path: &Path) -> Option<&'static str> {
    let ext = ext_of(path)?;
    if IMAGE_EXT.contains(&ext.as_str()) {
        Some("image")
    } else if VIDEO_EXT.contains(&ext.as_str()) {
        Some("video")
    } else {
        None
    }
}

pub fn is_raw(path: &Path) -> bool {
    ext_of(path).map(|e| RAW_EXT.contains(&e.as_str())).unwrap_or(false)
}

/// Cache key for derived files. Includes path, size, mtime, rotation and target size so a
/// changed file (or a rotation) can never be served a stale thumbnail.
pub fn cache_key(path: &str, bytes: i64, modified: i64, rotation: i64, size: u32) -> String {
    let mut h = Sha256::new();
    h.update(path.as_bytes());
    h.update(format!("\0{bytes}\0{modified}\0{rotation}\0{size}").as_bytes());
    let d = h.finalize();
    d.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------- process helpers

/// Run a command, killing it on cancel or timeout. Returns true only on exit status 0.
pub fn run_cancellable(mut cmd: Command, cancel: &AtomicBool, timeout: Duration) -> bool {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else { return false };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {}
            Err(_) => return false,
        }
        if cancel.load(Ordering::Relaxed) || start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        thread::sleep(Duration::from_millis(12));
    }
}

fn run_capture(mut cmd: Command, timeout: Duration) -> Option<Vec<u8>> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let mut out = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let buf = reader.join().ok()?;
                return if status.success() { Some(buf) } else { None };
            }
            Ok(None) => {}
            Err(_) => return None,
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(8));
    }
}


// ------------------------------------------------- host tools & AppImage environment

/// Variables whose value is a single path into the AppImage bundle (they are only meant for the
/// app's own GTK/WebKit stack, and break or confuse programs from the host system).
const BUNDLE_SINGLE: &[&str] = &[
    "PYTHONHOME", "GDK_PIXBUF_MODULE_FILE", "GIO_MODULE_DIR", "GIO_EXTRA_MODULES", "GI_TYPELIB_PATH", "GTK_PATH",
    "GTK_EXE_PREFIX", "GTK_DATA_PREFIX", "GTK_IM_MODULE_FILE", "GSETTINGS_SCHEMA_DIR", "GST_PLUGIN_SCANNER",
    "GST_REGISTRY_1_0", "GST_PLUGIN_SYSTEM_PATH", "GST_PLUGIN_SYSTEM_PATH_1_0", "GST_PLUGIN_PATH", "GST_PLUGIN_PATH_1_0",
];
/// Search-path style variables: only the entries that point into the bundle are dropped.
const BUNDLE_LISTS: &[&str] = &["LD_LIBRARY_PATH", "PATH", "XDG_DATA_DIRS", "PYTHONPATH", "PERLLIB", "PERL5LIB", "XDG_CONFIG_DIRS"];

/// The value `var` should have for a host program: `None` = unset. Pure, so it can be tested.
pub fn host_env_value(var: &str, value: &str, appdir: &str) -> Option<Option<String>> {
    let appdir = appdir.trim_end_matches('/');
    if appdir.is_empty() { return None; }
    let inside = |e: &str| { let e = e.trim_end_matches('/'); e == appdir || e.starts_with(&format!("{appdir}/")) };
    if BUNDLE_SINGLE.contains(&var) {
        // GStreamer path variables are also dropped when they merely end up empty/relative to the bundle.
        return if value.split(':').any(inside) { Some(None) } else { None };
    }
    if BUNDLE_LISTS.contains(&var) {
        let kept: Vec<&str> = value.split(':').filter(|e| !e.is_empty() && !inside(e)).collect();
        if kept.len() == value.split(':').filter(|e| !e.is_empty()).count() { return None; }
        return Some(if kept.is_empty() { None } else { Some(kept.join(":")) });
    }
    None
}

/// A `Command` for a program of the *host* system (ffmpeg, ffprobe, xdg-open...). When we run
/// from an AppImage the launcher points LD_LIBRARY_PATH, PATH, GST_*, PYTHONHOME... into the
/// bundle; inheriting that makes system tools load the bundle's (older) libpng/libjpeg/libxml2/
/// libzstd/openssl and fail on other distributions. So those entries are removed here.
pub fn host_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    if let Ok(appdir) = std::env::var("APPDIR") {
        for (k, v) in std::env::vars() {
            match host_env_value(&k, &v, &appdir) {
                Some(Some(nv)) => { cmd.env(&k, nv); }
                Some(None) => { cmd.env_remove(&k); }
                None => {}
            }
        }
        cmd.env_remove("LD_PRELOAD");
    }
    cmd
}

/// Run once at startup, before any web process exists: the bundle ships no GStreamer plugins, but
/// the launcher points GStreamer at the (missing) bundle folder, which hides the system's codecs
/// from the built-in video player. Remove only those variables so system GStreamer is found.
const GST_NEEDED: [&str; 3] = ["libgstapp.so", "libgstautodetect.so", "libgstplayback.so"];

fn gst_dir_complete(d: &std::path::Path) -> bool {
    GST_NEEDED.iter().all(|f| d.join(f).is_file())
}

/// Where the *host* keeps its GStreamer plugins (the AppImage bundles the libraries but no plugins).
fn gst_host_plugin_dir() -> Option<std::path::PathBuf> {
    let mut c: Vec<std::path::PathBuf> = Vec::new();
    for base in ["/usr/lib/x86_64-linux-gnu", "/usr/lib64", "/usr/lib", "/usr/lib/aarch64-linux-gnu", "/usr/lib/arm-linux-gnueabihf", "/usr/local/lib"] {
        c.push(std::path::Path::new(base).join("gstreamer-1.0"));
    }
    c.into_iter().find(|d| gst_dir_complete(d))
}

fn gst_host_scanner() -> Option<std::path::PathBuf> {
    for p in [
        "/usr/lib/x86_64-linux-gnu/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner",
        "/usr/lib/x86_64-linux-gnu/gstreamer-1.0/gst-plugin-scanner",
        "/usr/libexec/gstreamer-1.0/gst-plugin-scanner",
        "/usr/lib64/gstreamer-1.0/gst-plugin-scanner",
        "/usr/lib/gstreamer-1.0/gst-plugin-scanner",
    ] {
        let p = std::path::PathBuf::from(p);
        if p.is_file() { return Some(p); }
    }
    None
}

/// Inside an AppImage the launcher points GStreamer at plugin folders that were never bundled, so the
/// web engine finds no `appsink`/`autoaudiosink`, and WebKit's web process dies the moment a <video>
/// element is created (the whole window then looks frozen). Point it at the host's own plugins instead.
pub fn fix_gstreamer_env() {
    let Ok(appdir) = std::env::var("APPDIR") else { return };
    for k in ["GST_PLUGIN_SYSTEM_PATH", "GST_PLUGIN_SYSTEM_PATH_1_0", "GST_PLUGIN_PATH", "GST_PLUGIN_PATH_1_0", "GST_PLUGIN_SCANNER", "GST_REGISTRY_1_0", "GST_REGISTRY"] {
        if let Ok(v) = std::env::var(k) {
            if let Some(None) = host_env_value(k, &v, &appdir) {
                // keep a variable whose target really exists in the bundle (plugin folder with the
                // needed plugins, or the scanner executable); drop it when it points at nothing
                let usable = if k.ends_with("SCANNER") || k.starts_with("GST_REGISTRY") {
                    std::path::Path::new(&v).exists() || k.starts_with("GST_REGISTRY")
                } else {
                    v.split(':').any(|d| !d.is_empty() && gst_dir_complete(std::path::Path::new(d)))
                };
                if !usable { #[allow(unused_unsafe)] unsafe { std::env::remove_var(k) } }
            }
        }
    }
    // Software decoding only: hardware (VA-API) decoders talk to the system's GPU driver and can hang
    // the whole video pipeline on drivers that don't match the libraries shipped in the AppImage.
    #[allow(unused_unsafe)]
    unsafe {
        if std::env::var_os("GST_PLUGIN_FEATURE_RANK").is_none() {
            let off = ["vah264dec", "vah265dec", "vavp8dec", "vavp9dec", "vaav1dec", "vampeg2dec", "vavc1dec", "vajpegdec",
                       "vah264lpdec", "vah265lpdec", "vavp9lpdec", "vaapih264dec", "vaapih265dec", "vaapivp8dec", "vaapivp9dec",
                       "nvh264dec", "nvh265dec", "nvvp8dec", "nvvp9dec", "nvav1dec"];
            std::env::set_var("GST_PLUGIN_FEATURE_RANK", off.iter().map(|n| format!("{n}:0")).collect::<Vec<_>>().join(","));
        }
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }
    // The AppImage normally ships a complete, matching plugin set: use it as is. Mixing the bundled
    // GStreamer core with plugins from a (possibly newer) host install is what crashes the web process.
    let bundled_ok = ["GST_PLUGIN_SYSTEM_PATH_1_0", "GST_PLUGIN_SYSTEM_PATH"].iter().any(|k| {
        std::env::var(k).map(|v| v.split(':').any(|d| !d.is_empty() && gst_dir_complete(std::path::Path::new(d)))).unwrap_or(false)
    });
    if bundled_ok {
        #[allow(unused_unsafe)]
        unsafe {
            if let Some(h) = std::env::var_os("HOME") {
                let reg = std::path::Path::new(&h).join(".cache/gallerydesk-gst-registry.bin");
                if let Some(par) = reg.parent() { let _ = std::fs::create_dir_all(par); }
                std::env::set_var("GST_REGISTRY_1_0", reg);
            }
        }
        return;
    }
    if let Some(dir) = gst_host_plugin_dir() {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", &dir);
            std::env::set_var("GST_PLUGIN_SYSTEM_PATH", &dir);
            if let Some(sc) = gst_host_scanner() { std::env::set_var("GST_PLUGIN_SCANNER", sc); }
            // a private registry: never mix cache files written by other GStreamer builds
            if let Some(h) = std::env::var_os("HOME") {
                let reg = std::path::Path::new(&h).join(".cache/gallerydesk-gst-registry.bin");
                if let Some(par) = reg.parent() { let _ = std::fs::create_dir_all(par); }
                std::env::set_var("GST_REGISTRY_1_0", reg);
            }
        }
    }
}

/// Can the embedded web engine play video here? (Without these plugins it crashes instead of failing.)
pub fn gst_playback_ready() -> bool {
    for k in ["GST_PLUGIN_SYSTEM_PATH_1_0", "GST_PLUGIN_SYSTEM_PATH", "GST_PLUGIN_PATH"] {
        if let Ok(v) = std::env::var(k) {
            if v.split(':').filter(|e| !e.is_empty()).any(|e| gst_dir_complete(std::path::Path::new(e))) { return true; }
        }
    }
    gst_host_plugin_dir().is_some()
}

pub fn tool_available(name: &str) -> bool {
    host_command(name)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------- metadata

#[derive(Default, Debug, Clone)]
pub struct Meta {
    pub taken: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration: Option<f64>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<i64>,
    pub aperture: Option<f64>,
    pub exposure: Option<String>,
    pub focal: Option<f64>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// EXIF orientation 1..=8 (1 = upright). Not stored; used to render thumbnails.
    pub orientation: u32,
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Capture times are stored as "wall clock as if UTC" and displayed with timeZone "UTC", so
/// a photo shot at 23:30 local always lands on that calendar day regardless of the viewer's
/// time zone.
fn civil_to_secs(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> Option<i64> {
    if !(1990..=2100).contains(&y) || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s)
}

fn gps_coord(exif: &exif::Exif, tag: exif::Tag, ref_tag: exif::Tag) -> Option<f64> {
    let f = exif.get_field(tag, exif::In::PRIMARY)?;
    let exif::Value::Rational(v) = &f.value else { return None };
    if v.len() < 3 || v.iter().any(|r| r.denom == 0) {
        return None;
    }
    let deg = v[0].to_f64() + v[1].to_f64() / 60.0 + v[2].to_f64() / 3600.0;
    let r = exif.get_field(ref_tag, exif::In::PRIMARY)?;
    let sign = match r.display_value().to_string().trim_matches('"') {
        "S" | "W" => -1.0,
        _ => 1.0,
    };
    Some(deg * sign)
}

fn rational(exif: &exif::Exif, tag: exif::Tag) -> Option<f64> {
    let f = exif.get_field(tag, exif::In::PRIMARY)?;
    match &f.value {
        exif::Value::Rational(v) => v.first().filter(|r| r.denom != 0).map(|r| r.to_f64()),
        _ => None,
    }
}

fn text(exif: &exif::Exif, tag: exif::Tag) -> Option<String> {
    let f = exif.get_field(tag, exif::In::PRIMARY)?;
    let s = match &f.value {
        exif::Value::Ascii(v) => v
            .iter()
            .map(|b| String::from_utf8_lossy(b).trim_matches(char::from(0)).trim().to_string())
            .collect::<Vec<_>>()
            .join(" "),
        _ => f.display_value().to_string(),
    };
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

pub fn read_exif(path: &Path) -> Meta {
    let mut meta = Meta { orientation: 1, ..Default::default() };
    let Ok(file) = File::open(path) else { return meta };
    let mut rd = BufReader::new(file);
    // Real-world files often carry a slightly malformed IFD. Strict parsing would throw away
    // every field because of one bad one, so keep whatever parsed cleanly.
    let mut reader = exif::Reader::new();
    reader.continue_on_error(true);
    let exif = match reader.read_from_container(&mut rd) {
        Ok(e) => e,
        Err(e) => match e.distill_partial_result(|_ignored| {}) {
            Ok(e) => e,
            Err(_) => return meta,
        },
    };

    for tag in [exif::Tag::DateTimeOriginal, exif::Tag::DateTimeDigitized, exif::Tag::DateTime] {
        if let Some(f) = exif.get_field(tag, exif::In::PRIMARY) {
            if let exif::Value::Ascii(v) = &f.value {
                if let Some(first) = v.first() {
                    if let Ok(dt) = exif::DateTime::from_ascii(first) {
                        meta.taken = civil_to_secs(
                            dt.year as i64, dt.month as i64, dt.day as i64,
                            dt.hour as i64, dt.minute as i64, dt.second as i64,
                        );
                        if meta.taken.is_some() { break; }
                    }
                }
            }
        }
    }

    let uint = |t: exif::Tag| {
        exif.get_field(t, exif::In::PRIMARY).and_then(|f| f.value.get_uint(0)).map(|v| v as i64)
    };
    meta.width = uint(exif::Tag::PixelXDimension).or_else(|| uint(exif::Tag::ImageWidth));
    meta.height = uint(exif::Tag::PixelYDimension).or_else(|| uint(exif::Tag::ImageLength));
    if let Some(o) = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
    {
        if (1..=8).contains(&o) { meta.orientation = o; }
    }
    if (5..=8).contains(&meta.orientation) {
        std::mem::swap(&mut meta.width, &mut meta.height);
    }

    let make = text(&exif, exif::Tag::Make);
    let model = text(&exif, exif::Tag::Model);
    meta.camera = match (make, model) {
        (Some(mk), Some(md)) => {
            // Many cameras repeat the make inside the model ("Canon" / "Canon EOS R5").
            if md.to_lowercase().starts_with(&mk.to_lowercase()) { Some(md) } else { Some(format!("{mk} {md}")) }
        }
        (None, Some(md)) => Some(md),
        (Some(mk), None) => Some(mk),
        _ => None,
    };
    meta.lens = text(&exif, exif::Tag::LensModel);
    meta.iso = uint(exif::Tag::PhotographicSensitivity);
    meta.aperture = rational(&exif, exif::Tag::FNumber);
    meta.focal = rational(&exif, exif::Tag::FocalLength);
    meta.exposure = rational(&exif, exif::Tag::ExposureTime).filter(|v| *v > 0.0).map(|v| {
        if v < 1.0 { format!("1/{}s", (1.0 / v).round() as i64) } else { format!("{v:.1}s") }
    });
    meta.lat = gps_coord(&exif, exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef);
    meta.lon = gps_coord(&exif, exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef);
    if let (Some(la), Some(lo)) = (meta.lat, meta.lon) {
        if !(-90.0..=90.0).contains(&la) || !(-180.0..=180.0).contains(&lo) || (la == 0.0 && lo == 0.0) {
            meta.lat = None;
            meta.lon = None;
        }
    }
    meta
}

/// Dimensions / duration / creation time through ffprobe. Used for videos, and for images
/// whose format carries no EXIF dimensions (PNG, GIF, ...).
pub fn probe(path: &Path) -> Option<Meta> {
    let mut cmd = host_command("ffprobe");
    cmd.args([
        "-v", "error", "-select_streams", "v:0",
        "-show_entries", "stream=width,height:stream_side_data=rotation:format=duration:format_tags=creation_time",
        "-of", "json",
    ]).arg(path);
    let out = run_capture(cmd, Duration::from_secs(20))?;
    let v: serde_json::Value = serde_json::from_slice(&out).ok()?;
    let mut meta = Meta { orientation: 1, ..Default::default() };
    let stream = v.get("streams").and_then(|s| s.get(0));
    meta.width = stream.and_then(|s| s.get("width")).and_then(|x| x.as_i64());
    meta.height = stream.and_then(|s| s.get("height")).and_then(|x| x.as_i64());
    let rot = stream
        .and_then(|s| s.get("side_data_list"))
        .and_then(|l| l.get(0))
        .and_then(|d| d.get("rotation"))
        .and_then(|r| r.as_f64())
        .map(|r| r.abs() as i64)
        .unwrap_or(0);
    if rot == 90 || rot == 270 {
        std::mem::swap(&mut meta.width, &mut meta.height);
    }
    meta.duration = v
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(|d| d.as_str())
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d >= 0.0);
    if let Some(ct) = v.get("format").and_then(|f| f.get("tags")).and_then(|t| t.get("creation_time")).and_then(|c| c.as_str()) {
        // 2023-05-01T12:34:56.000000Z
        let b = ct.as_bytes();
        if b.len() >= 19 {
            let n = |a: usize, z: usize| ct.get(a..z).and_then(|s| s.parse::<i64>().ok());
            if let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) =
                (n(0, 4), n(5, 7), n(8, 10), n(11, 13), n(14, 16), n(17, 19))
            {
                meta.taken = civil_to_secs(y, mo, d, h, mi, s);
            }
        }
    }
    Some(meta)
}

// ---------------------------------------------------------------- rendering

fn orientation_filters(orientation: u32) -> Vec<&'static str> {
    match orientation {
        2 => vec!["hflip"],
        3 => vec!["hflip", "vflip"],
        4 => vec!["vflip"],
        5 => vec!["transpose=0"],
        6 => vec!["transpose=1"],
        7 => vec!["transpose=3"],
        8 => vec!["transpose=2"],
        _ => vec![],
    }
}

fn rotation_filters(rotation: i64) -> Vec<&'static str> {
    match rotation.rem_euclid(360) {
        90 => vec!["transpose=1"],
        180 => vec!["hflip", "vflip"],
        270 => vec!["transpose=2"],
        _ => vec![],
    }
}

/// Largest JPEG embedded in a RAW container (most RAW formats carry a full-size preview).
/// Pure byte scan, bounded to 256 MB, no external decoder required.
pub fn extract_embedded_jpeg(path: &Path) -> Option<Vec<u8>> {
    let len = fs::metadata(path).ok()?.len();
    if len > 256 * 1024 * 1024 { return None; }
    let data = fs::read(path).ok()?;
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 3 < data.len() {
        if data[i] == 0xFF && data[i + 1] == 0xD8 && data[i + 2] == 0xFF {
            starts.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut best: Option<(usize, usize)> = None;
    for (n, &s) in starts.iter().enumerate() {
        let limit = starts.get(n + 1).copied().unwrap_or(data.len());
        // Last EOI marker before the next SOI belongs to this image.
        let mut e = limit;
        while e > s + 2 {
            if data[e - 2] == 0xFF && data[e - 1] == 0xD9 { break; }
            e -= 1;
        }
        if e > s + 2 && e - s >= 20 * 1024 && best.map(|(bs, be)| e - s > be - bs).unwrap_or(true) {
            best = Some((s, e));
        }
    }
    best.map(|(s, e)| data[s..e].to_vec())
}

pub struct RenderOpts<'a> {
    pub src: &'a Path,
    pub out: &'a Path,
    pub long_edge: u32,
    pub video: bool,
    pub orientation: u32,
    pub rotation: i64,
    pub seek: Option<f64>,
    pub quality: u8,
}

fn ffmpeg_render(o: &RenderOpts, cancel: &AtomicBool) -> bool {
    let mut filters: Vec<String> = Vec::new();
    if !o.video {
        filters.extend(orientation_filters(o.orientation).into_iter().map(String::from));
    }
    filters.extend(rotation_filters(o.rotation).into_iter().map(String::from));
    let e = o.long_edge;
    filters.push(format!("scale=w='min(iw,{e})':h='min(ih,{e})':force_original_aspect_ratio=decrease"));

    let tmp = o.out.with_extension("part.jpg");
    let mut cmd = host_command("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-nostdin"]);
    if !o.video {
        // Orientation is applied explicitly above, so behaviour does not depend on whether
        // the installed ffmpeg build auto-rotates still images.
        cmd.arg("-noautorotate");
    }
    if let Some(t) = o.seek.filter(|t| *t > 0.0) {
        cmd.args(["-ss", &format!("{t:.2}")]);
    }
    cmd.arg("-i").arg(o.src);
    cmd.args(["-frames:v", "1", "-an", "-sn", "-vf", &filters.join(",")]);
    cmd.args(["-q:v", &o.quality.to_string()]).arg(&tmp);
    let ok = run_cancellable(cmd, cancel, Duration::from_secs(60));
    let produced = ok && fs::metadata(&tmp).map(|m| m.len() > 0).unwrap_or(false);
    if produced && fs::rename(&tmp, o.out).is_ok() {
        true
    } else {
        let _ = fs::remove_file(&tmp);
        false
    }
}

/// Render a JPEG derivative of `src` to `o.out`. Falls back to the embedded preview for RAW.
pub fn render(o: &RenderOpts, cancel: &AtomicBool) -> bool {
    if ffmpeg_render(o, cancel) {
        return true;
    }
    if !o.video && is_raw(o.src) && !cancel.load(Ordering::Relaxed) {
        if let Some(bytes) = extract_embedded_jpeg(o.src) {
            let embedded = o.out.with_extension("embedded.jpg");
            if fs::write(&embedded, bytes).is_ok() {
                let alt = RenderOpts { src: &embedded, ..*o };
                let ok = ffmpeg_render(&alt, cancel);
                let _ = fs::remove_file(&embedded);
                return ok;
            }
        }
    }
    false
}

pub fn thumb_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

/// Make a browser-friendly copy of a video (VP8 + Vorbis in WebM, at most 1080p) for the in-app
/// player when the system's media framework can't decode the original. The original is only read.
pub fn transcode_for_playback(src: &Path, out: &Path, cancel: &AtomicBool) -> bool {
    let part = out.with_extension("part.webm");
    let _ = fs::remove_file(&part);
    let mut cmd = host_command("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(src)
        .args(["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-dn",
               "-vf", "scale='min(1920,iw)':-2",
               "-c:v", "libvpx", "-deadline", "realtime", "-cpu-used", "8", "-b:v", "3M", "-crf", "12", "-pix_fmt", "yuv420p",
               "-c:a", "libvorbis", "-ac", "2", "-f", "webm"])
        .arg(&part);
    let ok = run_cancellable(cmd, cancel, Duration::from_secs(60 * 60));
    if ok && fs::rename(&part, out).is_ok() { return true; }
    let _ = fs::remove_file(&part);
    false
}
