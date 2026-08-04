//! Share resolution, directory listing, and **the path-containment layer**.
//!
//! Everything a client can name goes through `resolve_within`. Four principles
//! govern this file:
//!
//! 1. **Reject, never sanitize.** No `..`-popping, no character stripping.
//!    Anything not obviously benign is an error. Every "clean the path"
//!    implementation in the CVE record lost to an encoding its author didn't
//!    anticipate (`....//`, `%252e%252e%252f`, `..%c0%af`).
//! 2. **Never `Path::join` a user string.** `Path::new("D:/media").join("C:/x")`
//!    returns `C:/x` -- `join` *replaces* the base on an absolute component.
//!    We join validated segments one at a time.
//! 3. **Canonicalize + contain as a backstop**, not the primary defense. It
//!    catches symlinks, junctions, 8.3 short names and casing, but it is a
//!    syscall and it fails on paths that don't exist yet.
//! 4. **Decode exactly once.** axum's `Path`/`Query` extractors percent-decode
//!    for us. We never decode again -- a second decode is precisely how
//!    `%252e%252e` becomes `..`.

use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};

use crate::{
    media,
    models::{
        Breadcrumb, ListingEntry, ListingResponse, ResolvedShare, ServerSettings, Share,
    },
    utils,
};

// ---------------------------------------------------------------------------
// Rejection reasons
// ---------------------------------------------------------------------------

/// Why a client-supplied path was refused. Everything maps to 400 except
/// `NotFound` (404) and `Escapes` (403, logged as a denial).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathReject {
    Absolute,
    Traversal,
    DotSegment,
    Colon,
    IllegalChar(char),
    ReservedDevice(String),
    TrailingDotOrSpace,
    SegmentTooLong,
    NotNormalComponent,
    Escapes,
    NotFound,
}

impl PathReject {
    pub(crate) fn message(&self) -> String {
        match self {
            PathReject::Absolute => "absolute paths are not allowed".to_string(),
            PathReject::Traversal => "path traversal is not allowed".to_string(),
            PathReject::DotSegment => "'.' segments are not allowed".to_string(),
            PathReject::Colon => "':' is not allowed in a path".to_string(),
            PathReject::IllegalChar(c) => format!("illegal character {c:?} in path"),
            PathReject::ReservedDevice(s) => format!("reserved device name: {s}"),
            PathReject::TrailingDotOrSpace => {
                "path segments may not end with a dot or space".to_string()
            }
            PathReject::SegmentTooLong => "path segment is too long".to_string(),
            PathReject::NotNormalComponent => "path contains a non-normal component".to_string(),
            PathReject::Escapes => "path escapes the share root".to_string(),
            PathReject::NotFound => "not found".to_string(),
        }
    }

    /// 404 hides existence; 403 records a genuine escape attempt; the rest are
    /// malformed input.
    pub(crate) fn status_code(&self) -> u16 {
        match self {
            PathReject::NotFound => 404,
            PathReject::Escapes => 403,
            _ => 400,
        }
    }
}

// ---------------------------------------------------------------------------
// Segment validation
// ---------------------------------------------------------------------------

/// Windows device names. These resolve to a device no matter which directory
/// they appear in, and `CON.txt` / `nul` / `COM1 ` all reach the same device.
const WIN_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM0", "COM1", "COM2", "COM3", "COM4",
    "COM5", "COM6", "COM7", "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5",
    "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A segment names a device if the part before the FIRST dot, with trailing
/// spaces and dots removed, matches a reserved name case-insensitively. That
/// covers `CON`, `con.txt`, `COM1.tar.gz`, `NUL   ` and `aux.`.
pub(crate) fn is_reserved_device(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or("");
    let stem = stem.trim_end_matches([' ', '.']);
    WIN_RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem))
}

/// Validate one path segment. The caller has already split on both separators,
/// so a separator reaching here means the splitter was bypassed.
pub(crate) fn check_segment(seg: &str) -> Result<(), PathReject> {
    if seg == "." {
        return Err(PathReject::DotSegment);
    }
    if seg == ".." {
        return Err(PathReject::Traversal);
    }
    // NTFS's component limit is 255 UTF-16 units; byte length is a safe
    // over-estimate for rejection purposes.
    if seg.len() > 255 {
        return Err(PathReject::SegmentTooLong);
    }

    for ch in seg.chars() {
        match ch {
            // NUL truncates in any C API this eventually reaches; the other
            // control characters enable header and log injection.
            '\0'..='\x1f' | '\x7f' => return Err(PathReject::IllegalChar(ch)),
            // A colon is the NTFS Alternate Data Stream separator
            // ("notes.txt:hidden:$DATA") AND the drive separator ("C:foo",
            // which is drive-RELATIVE and resolves against C:'s current
            // directory -- a real escape). It is never legal in a Windows
            // filename, so nothing real is lost. On unix a colon IS legal; we
            // reject it there too so behaviour, and therefore the tests, are
            // identical on both platforms.
            ':' => return Err(PathReject::Colon),
            // Illegal on Windows. '*' and '?' additionally stop any downstream
            // API from treating the name as a glob.
            '<' | '>' | '"' | '|' | '?' | '*' => return Err(PathReject::IllegalChar(ch)),
            // The splitter should have consumed these.
            '/' | '\\' => return Err(PathReject::IllegalChar(ch)),
            _ => {}
        }
    }

    // Windows SILENTLY strips trailing dots and spaces: "secret.txt." opens
    // "secret.txt" and "admin " opens "admin". Allowing them means two distinct
    // URLs address one file, which defeats any name-based rule and would give
    // one file two thumbnail cache entries.
    if seg.ends_with(' ') || seg.ends_with('.') {
        return Err(PathReject::TrailingDotOrSpace);
    }

    if is_reserved_device(seg) {
        return Err(PathReject::ReservedDevice(seg.to_string()));
    }
    Ok(())
}

/// Split a client-supplied relative path into validated segments.
///
/// `rel` MUST already be percent-decoded exactly once. Do not decode here.
pub(crate) fn split_rel(rel: &str) -> Result<Vec<&str>, PathReject> {
    let rel = rel.trim();
    if rel.is_empty() {
        // The share root, which is a valid target.
        return Ok(Vec::new());
    }

    let bytes = rel.as_bytes();

    // Rooted forms, checked on the RAW string BEFORE splitting, so that
    // "\\?\C:\Windows" and "//server/share" cannot be reduced to innocent
    // segments by the empty-segment collapse below.
    if bytes[0] == b'/' || bytes[0] == b'\\' {
        // Covers "/etc/passwd", "\Windows", UNC "\\server\share",
        // the verbatim prefix "\\?\", and the device prefix "\\.\".
        return Err(PathReject::Absolute);
    }
    // "C:\Windows" (absolute) and "C:foo" (drive-relative -- resolves against
    // whatever the process's current directory on C: happens to be).
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        return Err(PathReject::Absolute);
    }

    let mut out = Vec::new();
    // Split on BOTH separators unconditionally. On unix `\` is a legal filename
    // character, so treating it as a separator loses a rare-but-legal name; NOT
    // treating it as one means `a\..\..\etc` is a single innocent-looking
    // segment on unix that becomes traversal the moment the path is used on
    // Windows or on a mounted SMB share. Uniformity wins.
    for seg in rel.split(['/', '\\']) {
        if seg.is_empty() {
            // Collapse "a//b" and a trailing "/".
            continue;
        }
        check_segment(seg)?;
        out.push(seg);
    }
    Ok(out)
}

/// Rejoin validated segments into the canonical wire form (forward slashes).
pub(crate) fn normalize_rel(rel: &str) -> Result<String, PathReject> {
    Ok(split_rel(rel)?.join("/"))
}

// ---------------------------------------------------------------------------
// Containment
// ---------------------------------------------------------------------------

/// Component-wise containment.
///
/// `Path::starts_with` is already component-wise -- so root `D:\media` correctly
/// rejects `D:\media-private\secret.txt`, the bug a naive string `starts_with`
/// would let through -- but it byte-compares, and Windows paths are
/// case-insensitive.
pub(crate) fn contained_in(root: &Path, child: &Path) -> bool {
    #[cfg(windows)]
    {
        let fold = |p: &Path| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
                .collect::<Vec<_>>()
        };
        let (r, c) = (fold(root), fold(child));
        r.len() <= c.len() && c[..r.len()] == r[..]
    }
    #[cfg(not(windows))]
    {
        child.starts_with(root)
    }
}

/// Resolve `rel` inside `root`, refusing anything that escapes.
///
/// `root` must already be `canonical_root` output. Returns the canonical
/// absolute path of an entry that exists.
pub(crate) fn resolve_within(root: &Path, rel: &str) -> Result<PathBuf, PathReject> {
    let segments = split_rel(rel)?;

    let mut candidate = root.to_path_buf();
    for seg in &segments {
        // One segment at a time. `push` with an absolute value would replace
        // the base -- which is exactly why `split_rel` rejects rooted forms and
        // why the whole user string is never pushed at once.
        candidate.push(seg);
    }

    // Belt and braces: every component past the root prefix must be Normal. A
    // ParentDir/RootDir/Prefix here means a check above was bypassed.
    let tail_is_clean = candidate
        .strip_prefix(root)
        .map(|tail| tail.components().all(|c| matches!(c, Component::Normal(_))))
        .unwrap_or(false);
    if !tail_is_clean {
        return Err(PathReject::NotNormalComponent);
    }

    // The real backstop: resolves symlinks, NTFS junctions, mount points and
    // 8.3 short names ("PROGRA~1"), and returns the on-disk casing.
    let resolved = fs::canonicalize(&candidate).map_err(|_| PathReject::NotFound)?;

    if !contained_in(root, &resolved) {
        return Err(PathReject::Escapes);
    }
    Ok(resolved)
}

/// Resolve a path for WRITING, where the leaf does not exist yet.
/// Canonicalizes the parent directory, then appends the validated leaf name.
pub(crate) fn resolve_new_within(
    root: &Path,
    rel_dir: &str,
    file_name: &str,
) -> Result<PathBuf, PathReject> {
    check_segment(file_name)?;
    let dir = resolve_within(root, rel_dir)?;
    if !dir.is_dir() {
        return Err(PathReject::NotFound);
    }
    let target = dir.join(file_name);
    // The parent is proven contained and `file_name` is proven a plain
    // segment, so `target` is contained by construction. Re-assert anyway.
    if !contained_in(root, &target) {
        return Err(PathReject::Escapes);
    }
    Ok(target)
}

/// Canonicalize a share root ONCE, at add-share / config-load time.
///
/// `std::fs::canonicalize`, NOT `dunce::canonicalize`. On Windows std returns
/// the verbatim `\\?\D:\Media` form, and that is wanted internally for three
/// reasons:
///
/// 1. Both sides of every containment check then carry the same prefix. `dunce`
///    strips the prefix only when the path is short enough, so a root of
///    `D:\Media` and a child of `\\?\D:\Media\<250 chars>` would fail
///    `starts_with` even though the child IS contained.
/// 2. Verbatim paths bypass `MAX_PATH`, so 300-character paths simply work with
///    no `longPathAware` manifest.
/// 3. The Win32 path normalizer is DISABLED for verbatim paths, so trailing
///    dots/spaces and device names are not reinterpreted at the syscall layer --
///    a second, independent line of defense behind `check_segment`.
///
/// Use `dunce::simplified` only when handing a path to the desktop UI or to
/// `explorer.exe`, both of which choke on the `\\?\` prefix.
pub(crate) fn canonical_root(path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("share path is empty"));
    }
    let root = fs::canonicalize(trimmed)
        .with_context(|| format!("share folder is not reachable: {trimmed}"))?;
    Ok(root)
}

/// Display form for the desktop UI and Explorer.
pub(crate) fn display_path(path: &Path) -> String {
    dunce::simplified(path).display().to_string()
}

// ---------------------------------------------------------------------------
// Share helpers
// ---------------------------------------------------------------------------

/// A share is servable only if it is enabled and its root still resolves.
pub(crate) fn is_servable(share: &ResolvedShare) -> bool {
    share.cfg.enabled && share.root_exists
}

/// Exact-name allowlist. Case-insensitive, because the casing the OS reports
/// is not the casing a link was built from.
fn passes_name_filter(share: &Share, name: &str) -> bool {
    share.include_names.is_empty()
        || share
            .include_names
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

pub(crate) fn can_upload_to(share: &ResolvedShare, settings: &ServerSettings) -> bool {
    settings.uploads_enabled
        && share.cfg.is_inbox
        && !share.cfg.read_only
        && !share.cfg.is_file
        && is_servable(share)
}

/// Hidden by name, by the OS hidden attribute, or by the share's extension
/// filters. Checked for every listed entry.
fn is_hidden(name: &str, path: &Path, settings: &ServerSettings) -> bool {
    if settings.show_hidden {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    if settings.hidden_names.iter().any(|h| h == &lower) {
        return true;
    }
    // Unix convention, and plenty of dotfiles land on Windows shares too.
    if name.starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
        if let Ok(meta) = path.metadata() {
            let attrs = meta.file_attributes();
            if attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0 {
                return true;
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
    false
}

fn passes_ext_filter(share: &Share, ext: &str) -> bool {
    if !share.exclude_ext.is_empty() && share.exclude_ext.iter().any(|e| e == ext) {
        return false;
    }
    if !share.include_ext.is_empty() && !share.include_ext.iter().any(|e| e == ext) {
        return false;
    }
    true
}

fn breadcrumbs_for(rel: &str) -> Vec<Breadcrumb> {
    let mut out = Vec::new();
    let mut acc = String::new();
    for seg in rel.split('/').filter(|s| !s.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        out.push(Breadcrumb {
            name: seg.to_string(),
            path: acc.clone(),
        });
    }
    out
}

fn parent_of(rel: &str) -> Option<String> {
    if rel.is_empty() {
        return None;
    }
    Some(match rel.rfind('/') {
        Some(idx) => rel[..idx].to_string(),
        None => String::new(),
    })
}

/// Sort a listing. Directories always lead -- on a phone, mixing them into a
/// size-sorted file list makes navigation unpredictable.
fn sort_entries(entries: &mut [ListingEntry], sort: &str, ascending: bool) {
    entries.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            return b.is_dir.cmp(&a.is_dir);
        }
        let ord = match sort {
            "size" => a.size.cmp(&b.size),
            "date" | "mtime" => a.mtime_ms.cmp(&b.mtime_ms),
            "kind" | "type" => a.kind.cmp(&b.kind).then_with(|| {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            }),
            // Case-insensitive so "apple" and "Banana" interleave the way a
            // file manager shows them, not with all capitals first.
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        };
        if ascending {
            ord
        } else {
            ord.reverse()
        }
    });
}

/// List one directory inside a share.
///
/// The requested directory is canonicalized once; its children are contained by
/// construction, so this is one `canonicalize` syscall per request rather than
/// one per entry.
pub(crate) fn list_directory(
    share: &ResolvedShare,
    rel: &str,
    settings: &ServerSettings,
    sort: Option<&str>,
    ascending: Option<bool>,
) -> Result<ListingResponse, PathReject> {
    if !is_servable(share) {
        return Err(PathReject::NotFound);
    }

    let rel = normalize_rel(rel)?;

    // A single-file share exposes exactly one entry at its root and nothing
    // below it, so `..`-free deep paths can't turn it into a folder share.
    if share.cfg.is_file {
        if !rel.is_empty() {
            return Err(PathReject::NotFound);
        }
        return Ok(single_file_listing(share, settings));
    }

    // A non-recursive share is browsable only at its top level.
    if !share.cfg.recursive && !rel.is_empty() {
        return Err(PathReject::NotFound);
    }

    let dir = resolve_within(&share.root, &rel)?;
    if !dir.is_dir() {
        return Err(PathReject::NotFound);
    }

    let read = fs::read_dir(&dir).map_err(|_| PathReject::NotFound)?;

    let mut entries = Vec::new();
    let mut dir_count = 0usize;
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;

    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        let path = item.path();

        // `symlink_metadata` does not follow, so a symlink loop cannot hang the
        // listing and a dangling link is still shown rather than erroring out.
        let link_meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_symlink = link_meta.file_type().is_symlink();

        // For everything else we do want the target's facts (size, is_dir).
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            // Dangling symlink, or a file another process holds exclusively.
            Err(_) => link_meta.clone(),
        };
        let is_dir = meta.is_dir();

        if is_hidden(&name, &path, settings) {
            continue;
        }
        if is_dir && !share.cfg.recursive {
            continue;
        }

        let ext = if is_dir {
            String::new()
        } else {
            utils::ext_of(&name)
        };
        if !is_dir && !passes_ext_filter(&share.cfg, &ext) {
            continue;
        }
        // A handoff share is rooted at a whole folder but exposes only the
        // files that were actually picked, so the rest of that folder stays
        // invisible -- and, via `resolve_file` below, unreachable.
        if !passes_name_filter(&share.cfg, &name) {
            continue;
        }

        // A symlinked directory could point outside the root. Listing it is
        // harmless (we never descend during a listing), but following it on the
        // next request must not escape -- `resolve_within` re-checks, so this
        // is only about not advertising an entry we would then refuse.
        if is_symlink {
            match fs::canonicalize(&path) {
                Ok(target) if contained_in(&share.root, &target) => {}
                _ => continue,
            }
        }

        let size = if is_dir { 0 } else { meta.len() };
        let mtime_ms = meta
            .modified()
            .map(utils::system_time_ms)
            .unwrap_or_default();

        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let kind = media::classify(is_dir, &ext);
        let playable = !is_dir && media::is_browser_playable(&kind, &ext);
        let thumb_url = if !is_dir && settings.thumbnails_enabled && media::can_thumbnail(&ext) {
            Some(format!(
                "/api/thumb?share={}&path={}",
                utils::url_encode(&share.cfg.id),
                utils::url_encode(&child_rel)
            ))
        } else {
            None
        };

        if is_dir {
            dir_count += 1;
        } else {
            file_count += 1;
            total_bytes += size;
        }

        entries.push(ListingEntry {
            name,
            is_dir,
            is_symlink,
            size,
            size_text: if is_dir {
                String::new()
            } else {
                utils::format_bytes(size)
            },
            mtime_ms,
            kind,
            ext,
            thumb_url,
            playable,
        });
    }

    let sort_key = sort.unwrap_or(&settings.default_sort);
    sort_entries(
        &mut entries,
        sort_key,
        ascending.unwrap_or(settings.default_sort_asc),
    );

    Ok(ListingResponse {
        share_id: share.cfg.id.clone(),
        share_name: share.cfg.name.clone(),
        is_inbox: share.cfg.is_inbox,
        can_upload: can_upload_to(share, settings),
        parent: parent_of(&rel),
        breadcrumbs: breadcrumbs_for(&rel),
        path: rel,
        entries,
        dir_count,
        file_count,
        total_bytes,
    })
}

/// A share that points at one file renders as a one-entry folder, so the
/// receiver UI needs no special case.
fn single_file_listing(share: &ResolvedShare, settings: &ServerSettings) -> ListingResponse {
    let name = share
        .root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| share.cfg.name.clone());
    let ext = utils::ext_of(&name);
    let meta = fs::metadata(&share.root).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mtime_ms = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .map(utils::system_time_ms)
        .unwrap_or_default();
    let kind = media::classify(false, &ext);
    let thumb_url = if settings.thumbnails_enabled && media::can_thumbnail(&ext) {
        Some(format!(
            "/api/thumb?share={}&path={}",
            utils::url_encode(&share.cfg.id),
            utils::url_encode(&name)
        ))
    } else {
        None
    };

    ListingResponse {
        share_id: share.cfg.id.clone(),
        share_name: share.cfg.name.clone(),
        is_inbox: false,
        can_upload: false,
        path: String::new(),
        parent: None,
        breadcrumbs: Vec::new(),
        entries: vec![ListingEntry {
            playable: media::is_browser_playable(&kind, &ext),
            name,
            is_dir: false,
            is_symlink: false,
            size,
            size_text: utils::format_bytes(size),
            mtime_ms,
            kind,
            ext,
            thumb_url,
        }],
        dir_count: 0,
        file_count: 1,
        total_bytes: size,
    }
}

/// Resolve a file for serving. Rejects directories -- `/files/` streams bytes,
/// and a directory there is a client bug, not something to guess about.
pub(crate) fn resolve_file(share: &ResolvedShare, rel: &str) -> Result<PathBuf, PathReject> {
    if !is_servable(share) {
        return Err(PathReject::NotFound);
    }

    // A single-file share addresses its one file by name; the root itself is
    // the file, so we never join.
    if share.cfg.is_file {
        let expected = share
            .root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let requested = normalize_rel(rel)?;
        // Windows filenames are case-insensitive, so accept either casing
        // rather than 404ing on a link the user typed by hand.
        if requested.eq_ignore_ascii_case(&expected) {
            return Ok(share.root.clone());
        }
        return Err(PathReject::NotFound);
    }

    let rel = normalize_rel(rel)?;
    if rel.is_empty() {
        return Err(PathReject::NotFound);
    }
    if !share.cfg.recursive && rel.contains('/') {
        return Err(PathReject::NotFound);
    }

    let path = resolve_within(&share.root, &rel)?;
    if !path.is_file() {
        return Err(PathReject::NotFound);
    }

    // Extension filters must apply to direct fetches too, or a filtered-out
    // file is still downloadable by anyone who guesses its name.
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if !passes_ext_filter(&share.cfg, &utils::ext_of(&name)) {
        return Err(PathReject::NotFound);
    }
    // Both filters apply to a direct fetch too. Without this, a file excluded
    // from the listing is still downloadable by anyone who guesses its name --
    // which for a handoff share means the whole containing folder.
    if !passes_name_filter(&share.cfg, &name) {
        return Err(PathReject::NotFound);
    }
    Ok(path)
}

/// Resolve a directory for zipping.
pub(crate) fn resolve_dir(share: &ResolvedShare, rel: &str) -> Result<PathBuf, PathReject> {
    if !is_servable(share) || share.cfg.is_file || !share.cfg.recursive {
        return Err(PathReject::NotFound);
    }
    let rel = normalize_rel(rel)?;
    let path = resolve_within(&share.root, &rel)?;
    if !path.is_dir() {
        return Err(PathReject::NotFound);
    }
    Ok(path)
}

/// Recursive stats for the desktop Shares table.
///
/// `follow_links(false)` means a symlinked directory is counted as an entry but
/// never descended, so a link loop cannot hang this and a link to `C:\` cannot
/// inflate the total by the whole drive.
pub(crate) fn index_share<F>(
    share: &ResolvedShare,
    mut on_progress: F,
) -> (u64, u64, u64, u64)
where
    F: FnMut(u64, u64),
{
    let mut files = 0u64;
    let mut dirs = 0u64;
    let mut bytes = 0u64;
    let mut skipped = 0u64;

    if !share.root_exists {
        return (0, 0, 0, 0);
    }
    if share.cfg.is_file {
        let size = fs::metadata(&share.root).map(|m| m.len()).unwrap_or(0);
        return (1, 0, size, 0);
    }

    let mut walker = walkdir::WalkDir::new(&share.root).follow_links(false);
    if !share.cfg.recursive {
        walker = walker.max_depth(1);
    }

    for entry in walker {
        match entry {
            Ok(entry) => {
                if entry.file_type().is_dir() {
                    // The root itself is not a child directory.
                    if entry.depth() > 0 {
                        dirs += 1;
                    }
                } else if entry.file_type().is_file() {
                    files += 1;
                    bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                }
                if (files + dirs) % 512 == 0 {
                    on_progress(files, bytes);
                }
            }
            // Unreadable directory (permissions, disconnected drive). Counted
            // and reported rather than aborting the whole index.
            Err(_) => skipped += 1,
        }
    }
    on_progress(files, bytes);
    (files, dirs, bytes, skipped)
}
