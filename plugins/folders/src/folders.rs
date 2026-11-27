//! The folders themselves: listing, finding and reading files, confined to the top level of each configured folder.

use std::fs::{File, Metadata};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::Engine;
use serde::Serialize;
use serde_json::{Value, json};

/// The most files one folder listing looks at. A Downloads folder with more is listed from the newest of these.
pub const MAX_SCAN: usize = 20_000;
/// Text files are sent up to this much, then cut with a note.
pub const MAX_TEXT: usize = 1024 * 1024;
/// An image this big or smaller can be downscaled; larger ones are refused.
pub const MAX_IMAGE_SOURCE: u64 = 50 * 1024 * 1024;
/// The longest side images are downscaled to unless the caller asks otherwise: what Claude itself scales images to.
pub const DEFAULT_MAX_PX: u32 = 1568;

#[derive(Debug, Clone)]
pub struct Folder {
    pub name: String,
    /// As configured, `~` expanded.
    pub path: PathBuf,
    /// Only its person writes to it (`--trusted <name>`): its files aren't marked as outside data.
    pub trusted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Image,
    Video,
    Pdf,
    Text,
    Archive,
    Other,
}

pub fn kind_of(name: &str) -> Kind {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "heif" | "tif" | "tiff" | "bmp" | "svg" => Kind::Image,
        "mov" | "mp4" | "m4v" | "webm" | "mkv" | "avi" => Kind::Video,
        "pdf" => Kind::Pdf,
        "txt" | "md" | "markdown" | "json" | "jsonl" | "csv" | "tsv" | "log" | "yaml" | "yml" | "toml" | "xml"
        | "html" | "htm" | "css" | "js" | "ts" | "py" | "rb" | "rs" | "go" | "sh" | "sql" | "ini" | "conf" | "eml"
        | "ics" | "vcf" | "srt" | "vtt" | "diff" | "patch" => Kind::Text,
        "zip" | "gz" | "tgz" | "tar" | "bz2" | "xz" | "7z" | "rar" | "dmg" | "pkg" | "zst" => Kind::Archive,
        _ => Kind::Other,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub modified: String,
    pub ago: String,
    pub kind: Kind,
    /// Kept in the cloud (iCloud Drive, Dropbox and other File Provider folders) and not downloaded to this Mac.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub not_downloaded: bool,
    #[serde(skip)]
    pub mtime: SystemTime,
}

#[derive(Debug, Default, Serialize)]
pub struct Listing {
    pub files: Vec<Entry>,
    /// How many files matched, before `limit` and `offset`.
    pub total: usize,
    /// Symbolic links, which are never followed.
    #[serde(skip_serializing_if = "is_zero")]
    pub skipped_links: usize,
    /// The folder had more than `MAX_SCAN` entries and only that many were looked at.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// An error for the agent: a plain sentence saying what went wrong and, where there's one, what to do.
#[derive(Debug)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn refuse<T>(msg: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal(msg.into()))
}

/// Explain an I/O error on a folder. macOS answers "Operation not permitted" when its privacy protection keeps the
/// process out of Downloads, Desktop or Documents.
pub fn explain(folder: &Folder, e: &std::io::Error) -> Refusal {
    let path = folder.path.display();
    match e.kind() {
        std::io::ErrorKind::NotFound => {
            Refusal(format!("the {} folder ({path}) doesn't exist on this machine", folder.name))
        }
        std::io::ErrorKind::PermissionDenied if cfg!(target_os = "macos") => Refusal(format!(
            "macOS refused access to the {} folder ({path}). Its owner gives the Fleetlayer node access once: System \
             Settings > Privacy & Security > Files and Folders (or Full Disk Access), then restarts the node",
            folder.name
        )),
        std::io::ErrorKind::PermissionDenied => {
            Refusal(format!("no permission to read the {} folder ({path})", folder.name))
        }
        _ => Refusal(format!("can't read the {} folder ({path}): {e}", folder.name)),
    }
}

impl Folder {
    /// The folder's real path, resolved at each use: a missing folder or one macOS keeps us out of is an error that
    /// says so, not a crash at startup.
    pub fn canonical(&self) -> Result<PathBuf, Refusal> {
        let c = std::fs::canonicalize(&self.path).map_err(|e| explain(self, &e))?;
        if !c.is_dir() {
            return refuse(format!("the {} folder ({}) isn't a directory", self.name, self.path.display()));
        }
        Ok(c)
    }

    /// Every visible regular file at the top level, newest first.
    pub fn scan(&self) -> Result<Listing, Refusal> {
        let dir = self.canonical()?;
        let rd = std::fs::read_dir(&dir).map_err(|e| explain(self, &e))?;
        let now = SystemTime::now();
        let mut out = Listing::default();
        let mut seen = 0usize;
        for item in rd {
            let Ok(item) = item else { continue };
            seen += 1;
            if seen > MAX_SCAN {
                out.truncated = true;
                break;
            }
            let Some(name) = item.file_name().to_str().map(str::to_string) else { continue };
            if name.starts_with('.') {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(item.path()) else { continue };
            if meta.file_type().is_symlink() {
                out.skipped_links += 1;
                continue;
            }
            if !meta.file_type().is_file() {
                continue;
            }
            out.files.push(entry(name, &meta, now));
        }
        out.files.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.name.cmp(&b.name)));
        out.total = out.files.len();
        Ok(out)
    }

    /// Open one file by name, refusing anything but a regular file directly inside the folder.
    pub fn open(&self, name: &str) -> Result<(File, Metadata), Refusal> {
        check_name(name)?;
        let dir = self.canonical()?;
        let path = dir.join(name);
        let before = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return refuse(format!("there's no file called {name:?} in {}", self.name));
            }
            Err(e) => return Err(explain(self, &e)),
        };
        if before.file_type().is_symlink() {
            return refuse(format!("{name:?} in {} is a symbolic link, which this tool never follows", self.name));
        }
        if !before.file_type().is_file() {
            return refuse(format!("{name:?} in {} isn't a file", self.name));
        }
        if is_dataless(&before) {
            return refuse(format!(
                "{name:?} in {} is kept in the cloud and isn't downloaded to this machine. Its owner can download it \
                 (open it in Finder) and ask again",
                self.name
            ));
        }
        let file = open_nofollow(&path).map_err(|e| explain(self, &e))?;
        let meta = file.metadata().map_err(|e| explain(self, &e))?;
        // The file opened must be the one looked at: nothing swapped in between.
        if meta.dev() != before.dev() || meta.ino() != before.ino() || !meta.is_file() {
            return refuse(format!("{name:?} in {} changed while it was being opened; ask again", self.name));
        }
        Ok((file, meta))
    }
}

fn entry(name: String, meta: &Metadata, now: SystemTime) -> Entry {
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    Entry {
        kind: kind_of(&name),
        not_downloaded: is_dataless(meta),
        size: meta.len(),
        modified: timestamp(mtime),
        ago: ago(now.duration_since(mtime).unwrap_or_default()),
        mtime,
        name,
    }
}

/// A name the caller may ask for: one path component, not hidden.
pub fn check_name(name: &str) -> Result<(), Refusal> {
    if name.is_empty() {
        return refuse("name is empty: give a file name from list, latest or find");
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return refuse(format!("{name:?} isn't a file name: only files directly in the folder can be read"));
    }
    if name.starts_with('.') {
        return refuse(format!("{name:?} is a hidden file, which this tool doesn't read"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn is_dataless(meta: &Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    const SF_DATALESS: u32 = 0x4000_0000;
    meta.st_flags() & SF_DATALESS != 0
}

#[cfg(not(target_os = "macos"))]
fn is_dataless(_meta: &Metadata) -> bool {
    false
}

#[cfg(target_os = "macos")]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(target_os = "linux")]
const O_NOFOLLOW: i32 = 0o400000;

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn open_nofollow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).custom_flags(O_NOFOLLOW).open(path)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn open_nofollow(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

pub fn timestamp(t: SystemTime) -> String {
    let ms = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
    let Ok(ts) = jiff::Timestamp::from_millisecond(ms) else { return String::new() };
    ts.to_zoned_datetime(jiff::tz::TimeZone::system()).strftime("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

pub fn ago(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => "just now".into(),
        60..3600 => plural(s / 60, "minute"),
        3600..86_400 => plural(s / 3600, "hour"),
        _ => plural(s / 86_400, "day"),
    }
}

fn plural(n: u64, unit: &str) -> String {
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// `since`: `30m`, `2h`, `3d`, `1w`, a date (`2026-11-27`, midnight here) or a timestamp (`2026-11-27T09:00:00Z`).
pub fn parse_since(s: &str, now: SystemTime) -> Result<SystemTime, Refusal> {
    let s = s.trim();
    let bad = || Refusal(format!("since {s:?} isn't a time: use 30m, 2h, 3d, 1w, a date (2026-11-27) or a timestamp"));
    if let Some(unit) = s.chars().last().filter(|c| c.is_ascii_alphabetic())
        && let Ok(n) = s[..s.len() - 1].parse::<u64>()
    {
        let secs = match unit {
            's' => n,
            'm' => n * 60,
            'h' => n * 3600,
            'd' => n * 86_400,
            'w' => n * 7 * 86_400,
            _ => return Err(bad()),
        };
        return Ok(now.checked_sub(Duration::from_secs(secs)).unwrap_or(SystemTime::UNIX_EPOCH));
    }
    if let Ok(ts) = s.parse::<jiff::Timestamp>() {
        return Ok(system_time(ts));
    }
    if let Ok(date) = s.parse::<jiff::civil::Date>() {
        let zoned = date.to_zoned_datetime(jiff::tz::TimeZone::system()).map_err(|_| bad())?;
        return Ok(system_time(zoned.timestamp()));
    }
    Err(bad())
}

fn system_time(ts: jiff::Timestamp) -> SystemTime {
    let ms = ts.as_millisecond();
    if ms >= 0 { SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64) } else { SystemTime::UNIX_EPOCH }
}

/// Case-insensitive match: `*` and `?` as wildcards, or a plain substring when the pattern has neither.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    if !p.contains(&'*') && !p.contains(&'?') {
        return name.to_lowercase().contains(&pattern.to_lowercase());
    }
    // Iterative glob with backtracking to the last star.
    let (mut pi, mut ni, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ---------------------------------------------------------------- reading

pub struct ReadOptions {
    /// The longest side to downscale images to; 0 sends the original.
    pub max_px: u32,
    /// The most bytes one reply may carry (before base64).
    pub max_bytes: u64,
}

const IMAGE_MIME: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    if let Some((_, m)) = IMAGE_MIME.iter().find(|(e, _)| *e == ext) {
        return m;
    }
    match ext.as_str() {
        "pdf" => "application/pdf",
        "heic" | "heif" => "image/heic",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "json" => "application/json",
        "csv" => "text/csv",
        "mov" => "video/quicktime",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}

fn human(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{} KB", n.div_ceil(1024))
    } else {
        format!("{n} bytes")
    }
}

fn read_all(file: File, limit: u64) -> Result<Vec<u8>, Refusal> {
    let mut buf = Vec::new();
    file.take(limit + 1).read_to_end(&mut buf).map_err(|e| Refusal(format!("reading failed: {e}")))?;
    Ok(buf)
}

/// Read a file into MCP content items: an image as image content (downscaled), text as text, anything else as an
/// embedded resource with base64 data.
pub fn read(folder: &Folder, name: &str, opts: &ReadOptions) -> Result<Vec<Value>, Refusal> {
    let (file, meta) = folder.open(name)?;
    let size = meta.len();
    let b64 = base64::engine::general_purpose::STANDARD;
    let summary = |extra: &str| {
        format!(
            "{name} in {} ({}, modified {}){extra}",
            folder.name,
            human(size),
            timestamp(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH))
        )
    };
    let mime = mime_of(name);

    if mime.starts_with("image/") && IMAGE_MIME.iter().any(|(_, m)| *m == mime) {
        if size > MAX_IMAGE_SOURCE {
            return refuse(format!(
                "{name} is {}, more than the {} an image may be",
                human(size),
                human(MAX_IMAGE_SOURCE)
            ));
        }
        let bytes = read_all(file, MAX_IMAGE_SOURCE)?;
        match prepare_image(&bytes, mime, opts) {
            Ok(img) => {
                let note = match img.scaled_from {
                    Some((w, h)) => {
                        format!(", {w}x{h} sent as {}x{} ({})", img.width, img.height, human(img.data.len() as u64))
                    }
                    None => format!(", {}x{}", img.width, img.height),
                };
                return Ok(vec![
                    json!({"type": "text", "text": summary(&note)}),
                    json!({"type": "image", "data": b64.encode(&img.data), "mimeType": img.mime}),
                ]);
            }
            // A file that doesn't decode (or a hostile one over the limits) is sent as a file, never a crash.
            Err(why) => {
                if bytes.len() as u64 > opts.max_bytes {
                    return refuse(format!(
                        "{name} can't be previewed ({why}) and at {} is too big to send",
                        human(size)
                    ));
                }
                return Ok(vec![
                    json!({"type": "text", "text": summary(&format!(": can't preview it ({why}), sent as a file"))}),
                    resource(folder, name, mime, &b64.encode(&bytes)),
                ]);
            }
        }
    }

    if size > opts.max_bytes && kind_of(name) != Kind::Text {
        return refuse(format!(
            "{name} is {}, more than the {} this tool sends in one reply",
            human(size),
            human(opts.max_bytes)
        ));
    }
    let bytes = read_all(file, opts.max_bytes.max(MAX_TEXT as u64))?;
    let head = &bytes[..bytes.len().min(8192)];
    let looks_text = !head.contains(&0);
    if looks_text {
        let cut = bytes.len() > MAX_TEXT;
        let mut end = bytes.len().min(MAX_TEXT);
        // Cut at a character boundary.
        while end > 0 && end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        if let Ok(text) = std::str::from_utf8(&bytes[..end]) {
            let note = if cut { format!(": the first {} of it", human(end as u64)) } else { String::new() };
            return Ok(vec![json!({"type": "text", "text": summary(&note)}), json!({"type": "text", "text": text})]);
        }
    }
    if bytes.len() as u64 > opts.max_bytes {
        return refuse(format!(
            "{name} is {}, more than the {} this tool sends in one reply",
            human(size),
            human(opts.max_bytes)
        ));
    }
    Ok(vec![json!({"type": "text", "text": summary("")}), resource(folder, name, mime, &b64.encode(&bytes))])
}

fn resource(folder: &Folder, name: &str, mime: &str, blob: &str) -> Value {
    json!({"type": "resource", "resource": {
        "uri": format!("folder://{}/{}", folder.name, percent(name)), "mimeType": mime, "blob": blob}})
}

fn percent(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub struct Prepared {
    pub data: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub scaled_from: Option<(u32, u32)>,
}

/// Decode with limits (Downloads holds files from strangers), downscale to `max_px` on the longest side, and encode
/// as PNG (screenshots keep their sharp text) or JPEG for photos. The original bytes are kept when no scaling is
/// needed and they fit.
pub fn prepare_image(bytes: &[u8], mime: &'static str, opts: &ReadOptions) -> Result<Prepared, String> {
    let mut reader =
        image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    reader.limits(decode_limits());
    let (w, h) = reader.into_dimensions().map_err(|e| e.to_string())?;
    let needs_scaling = opts.max_px > 0 && w.max(h) > opts.max_px;
    if !needs_scaling && bytes.len() as u64 <= opts.max_bytes {
        return Ok(Prepared { data: bytes.to_vec(), mime, width: w, height: h, scaled_from: None });
    }
    let mut reader =
        image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    reader.limits(decode_limits());
    let img = reader.decode().map_err(|e| e.to_string())?;
    let target = if opts.max_px > 0 { opts.max_px } else { w.max(h) };
    let img = if w.max(h) > target { img.resize(target, target, image::imageops::FilterType::Triangle) } else { img };
    let (nw, nh) = (img.width(), img.height());
    let mut out = Vec::new();
    let mime: &'static str = if mime == "image/jpeg" {
        let rgb = img.to_rgb8();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
            .encode_image(&rgb)
            .map_err(|e| e.to_string())?;
        "image/jpeg"
    } else {
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).map_err(|e| e.to_string())?;
        // A photo saved as PNG can stay large: JPEG it then.
        if out.len() as u64 > opts.max_bytes.min(5 * 1024 * 1024) {
            out.clear();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
                .encode_image(&img.to_rgb8())
                .map_err(|e| e.to_string())?;
            "image/jpeg"
        } else {
            "image/png"
        }
    };
    if out.len() as u64 > opts.max_bytes {
        return Err(format!("still {} after scaling", human(out.len() as u64)));
    }
    Ok(Prepared { data: out, mime, width: nw, height: nh, scaled_from: ((nw, nh) != (w, h)).then_some((w, h)) })
}

/// Downloads holds files from strangers: a hostile image mustn't exhaust memory.
fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(512 * 1024 * 1024);
    limits
}
