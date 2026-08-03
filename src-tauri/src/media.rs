//! File-kind classification, MIME resolution, and the thumbnail pipeline.
//!
//! The thumbnail cache lives on disk in `app_cache_dir()/thumbs` -- that is a
//! cache directory, not a database, so it stays inside the "JSON config only"
//! constraint. Keys are SHA-256 of `(canonical path, mtime, size, edge,
//! quality)`, so a re-saved file misses the cache and a toolchain upgrade does
//! not invalidate it.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use image::ImageReader;

use crate::{models::THUMB_DIR_NAME, utils};

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "avif", "heic", "heif", "svg",
    "ico", "jfif",
];
const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "webm", "ogv", "mov", "mkv", "avi", "wmv", "flv", "mpg", "mpeg", "3gp", "ts",
    "m2ts",
];
const AUDIO_EXTS: &[&str] = &[
    "mp3", "m4a", "aac", "ogg", "oga", "opus", "wav", "flac", "wma", "aiff", "mid", "midi",
];
const ARCHIVE_EXTS: &[&str] = &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso", "var"];
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "json", "xml", "csv", "log", "ini", "cfg", "yml", "yaml", "html", "htm", "css",
    "js", "ts", "rs", "py", "c", "cpp", "h", "java", "sh",
];

/// Extensions the *browser* can actually decode. `.mkv`, `.avi`, `.wmv` and
/// `.heic` classify as media but no browser plays them, and a broken `<video>`
/// element is a worse experience than an honest download card. `.mov` is
/// marked optimistically: Safari plays it, Chrome plays the H.264 ones.
const PLAYABLE_VIDEO: &[&str] = &["mp4", "m4v", "webm", "ogv", "mov"];
const PLAYABLE_AUDIO: &[&str] = &["mp3", "m4a", "aac", "ogg", "oga", "opus", "wav", "flac"];
const PLAYABLE_IMAGE: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "ico", "jfif"];

/// Extensions our thumbnailer can decode. Deliberately narrower than
/// `IMAGE_EXTS`: no AVIF/HEIC (those need C libraries), and no SVG (vector, and
/// rasterizing untrusted SVG is a different risk surface entirely).
const THUMBNAILABLE: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "jfif"];

pub(crate) fn classify(is_dir: bool, ext: &str) -> String {
    if is_dir {
        return "dir".to_string();
    }
    let e = ext.to_ascii_lowercase();
    if IMAGE_EXTS.contains(&e.as_str()) {
        "image"
    } else if VIDEO_EXTS.contains(&e.as_str()) {
        "video"
    } else if AUDIO_EXTS.contains(&e.as_str()) {
        "audio"
    } else if e == "pdf" {
        "pdf"
    } else if ARCHIVE_EXTS.contains(&e.as_str()) {
        "archive"
    } else if TEXT_EXTS.contains(&e.as_str()) {
        "text"
    } else {
        "other"
    }
    .to_string()
}

pub(crate) fn is_browser_playable(kind: &str, ext: &str) -> bool {
    let e = ext.to_ascii_lowercase();
    match kind {
        "video" => PLAYABLE_VIDEO.contains(&e.as_str()),
        "audio" => PLAYABLE_AUDIO.contains(&e.as_str()),
        "image" => PLAYABLE_IMAGE.contains(&e.as_str()),
        "pdf" => true,
        _ => false,
    }
}

pub(crate) fn can_thumbnail(ext: &str) -> bool {
    THUMBNAILABLE.contains(&ext.to_ascii_lowercase().as_str())
}

/// MIME for the `Content-Type` header.
///
/// Getting this right is mandatory, not cosmetic: **iOS refuses to play a video
/// served as `application/octet-stream`**, silently, with no error in the page.
pub(crate) fn mime_for(path: &Path) -> String {
    let guess = mime_guess::from_path(path).first_raw();
    if let Some(mime) = guess {
        return mime.to_string();
    }
    // mime_guess misses a few container formats that browsers do handle.
    match utils::ext_of(&path.to_string_lossy()).as_str() {
        "mkv" => "video/x-matroska".to_string(),
        "m2ts" | "ts" => "video/mp2t".to_string(),
        "opus" => "audio/ogg".to_string(),
        "heic" | "heif" => "image/heic".to_string(),
        "avif" => "image/avif".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Thumbnail cache
// ---------------------------------------------------------------------------

pub(crate) fn thumb_dir(app: &tauri::AppHandle) -> Result<PathBuf> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_cache_dir()
        .context("failed to resolve app cache dir")?
        .join(THUMB_DIR_NAME);
    fs::create_dir_all(&dir).context("failed to create thumbnail cache dir")?;
    Ok(dir)
}

/// Cache key from the CANONICAL path plus the facts that change when the file
/// changes. Canonical, so `PROGRA~1/x.jpg` and `Program Files/x.jpg` -- the same
/// file by two names -- share one entry instead of decoding twice.
pub(crate) fn cache_key(canonical: &Path, mtime_ms: u64, size: u64, edge: u32, quality: u8) -> String {
    let material = format!(
        "{}|{}|{}|{}|{}",
        canonical.to_string_lossy(),
        mtime_ms,
        size,
        edge,
        quality
    );
    utils::sha256_hex(&material)
}

pub(crate) fn cache_path(dir: &Path, key: &str) -> PathBuf {
    // Two-character shard: a 20k-image library in one flat directory makes
    // every `fs::metadata` on it slower, and Explorer unusable.
    dir.join(&key[..2]).join(format!("{key}.jpg"))
}

/// Generate (or read from cache) a JPEG thumbnail. Returns the encoded bytes.
///
/// The caller must hold a permit from `AppState::thumb_permits` -- a folder of
/// 5,000 photos would otherwise start 5,000 full-resolution decodes at once.
pub(crate) fn thumbnail(
    source: &Path,
    cache_dir: &Path,
    edge: u32,
    quality: u8,
) -> Result<(Vec<u8>, String)> {
    let meta = fs::metadata(source).context("thumbnail source is unreadable")?;
    let mtime_ms = meta
        .modified()
        .map(utils::system_time_ms)
        .unwrap_or_default();
    let key = cache_key(source, mtime_ms, meta.len(), edge, quality);
    let cached = cache_path(cache_dir, &key);

    if let Ok(bytes) = fs::read(&cached) {
        if !bytes.is_empty() {
            return Ok((bytes, key));
        }
    }

    let bytes = render(source, edge, quality)?;

    // Best-effort write: a full disk or a locked cache file must not fail the
    // request -- we already have the bytes the client asked for.
    if let Some(parent) = cached.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&cached, &bytes);

    Ok((bytes, key))
}

fn render(source: &Path, edge: u32, quality: u8) -> Result<Vec<u8>> {
    let mut reader = ImageReader::open(source)
        .with_context(|| format!("failed to open image {}", source.display()))?
        .with_guessed_format()
        .context("failed to detect image format")?;

    // A malicious or corrupt file can declare enormous dimensions. Without a
    // cap, decoding one 60000x60000 PNG allocates ~14 GB and takes the whole
    // app down -- and shares can contain files the host never inspected.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);

    let image = reader.decode().context("failed to decode image")?;

    // `thumbnail` is a fast box filter for the big reduction; Lanczos3 on a
    // 48 MP photo costs seconds per image for a 320px result nobody inspects.
    let thumb = image.thumbnail(edge, edge);

    let mut out = Vec::new();
    let rgb = thumb.to_rgb8();
    let mut encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(30, 95));
    encoder
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| anyhow!("failed to encode thumbnail: {e}"))?;

    Ok(out)
}

// ---------------------------------------------------------------------------
// Cache maintenance
// ---------------------------------------------------------------------------

pub(crate) fn cache_stats(dir: &Path) -> (u64, u64) {
    let mut count = 0u64;
    let mut bytes = 0u64;
    for entry in walkdir::WalkDir::new(dir).follow_links(false).into_iter().flatten() {
        if entry.file_type().is_file() {
            count += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    (count, bytes)
}

pub(crate) fn clear_cache(dir: &Path) -> Result<u64> {
    let (_, bytes) = cache_stats(dir);
    for entry in fs::read_dir(dir).context("failed to read thumbnail cache")?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(bytes)
}

/// Trim the cache to `max_bytes`, oldest-accessed first.
///
/// Called after a prewarm rather than on every request: walking the cache costs
/// a stat per file, which is not something to pay on a thumbnail fetch.
pub(crate) fn evict_to(dir: &Path, max_bytes: u64) -> Result<u64> {
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total = 0u64;

    for entry in walkdir::WalkDir::new(dir).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let len = meta.len();
        total += len;
        // `accessed` is disabled on many Windows volumes, so fall back to
        // `modified` -- for a write-once cache the two are the same anyway.
        let stamp = meta
            .accessed()
            .or_else(|_| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        files.push((entry.path().to_path_buf(), len, stamp));
    }

    if total <= max_bytes {
        return Ok(0);
    }

    files.sort_by_key(|(_, _, stamp)| *stamp);
    let mut freed = 0u64;
    for (path, len, _) in files {
        if total - freed <= max_bytes {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            freed += len;
        }
    }
    Ok(freed)
}
