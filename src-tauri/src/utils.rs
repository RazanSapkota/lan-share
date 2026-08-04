//! Small shared helpers. `format_bytes` is carried over verbatim from the
//! reference app so byte strings read identically across both tools.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

pub(crate) fn format_bytes(size: u64) -> String {
    let units = ["B", "KB", "MB", "GB"];
    let mut value = size as f64;
    for unit in units {
        if value < 1024.0 || unit == "GB" {
            return format!("{value:.1} {unit}");
        }
        value /= 1024.0;
    }
    format!("{size} B")
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Milliseconds since the epoch for a filesystem timestamp. Pre-1970 mtimes
/// (some archives set them) clamp to 0 rather than panicking on the negative.
pub(crate) fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Stable across Rust releases, unlike `std::hash::DefaultHasher`, which is
/// explicitly documented as unstable. The thumbnail cache is on disk and
/// outlives a toolchain upgrade, so it needs a hash that does too.
pub(crate) fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Standard base64, for `data:` URLs.
///
/// Hand-rolled for the same reason everything else here is: it is twenty lines
/// against a dependency, and the only caller is the thumbnail proxy. Padded and
/// with the `+/` alphabet, unlike `discovery`'s unpadded base64url -- a data URL
/// has to be the flavour a browser decodes.
pub(crate) fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Lowercase extension without the dot. `""` when there is none.
pub(crate) fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Split a filename into (stem, ".ext"). The extension keeps its dot so
/// rejoining is a plain concat, and a dotfile like `.gitignore` stays whole.
pub(crate) fn split_name(name: &str) -> (String, String) {
    match name.rfind('.') {
        // A leading dot is a hidden-file marker, not an extension separator.
        Some(idx) if idx > 0 => (name[..idx].to_string(), name[idx..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

/// Pick a non-colliding destination inside `dir`.
///
/// Never overwrites: a receiver uploading `photo.jpg` twice must not be able to
/// clobber the host's copy. Returns `None` if 999 variants are all taken, which
/// the caller surfaces as a rejection rather than looping forever.
pub(crate) fn unique_destination(dir: &Path, file_name: &str) -> Option<PathBuf> {
    let direct = dir.join(file_name);
    if !direct.exists() {
        return Some(direct);
    }
    let (stem, ext) = split_name(file_name);
    for n in 2..=999u32 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Percent-encode a path segment for use in a URL query value.
///
/// `NON_ALPHANUMERIC` is deliberately blunt: it encodes `/`, `&`, `=`, `+`,
/// `#`, `?` and every other character a filename may legally contain on the
/// platforms we serve, so nothing a user names a file can restructure the URL.
pub(crate) fn url_encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Percent-encode one path segment, keeping the RFC 3986 unreserved set
/// literal.
///
/// The difference from [`url_encode`] is the dot, and it is not cosmetic: a
/// media player chooses its demuxer from the extension it can see in the URL,
/// so `movie%2Emkv` is a file of unknown type. Same class of bug as the one
/// [`rfc8187_encode`] exists to avoid, one layer along.
pub(crate) fn url_encode_segment(value: &str) -> String {
    const UNRESERVED_EXTRAS: &[u8] = b"-._~";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || UNRESERVED_EXTRAS.contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Percent-encode for the `filename*` parameter of `Content-Disposition`
/// (RFC 8187 `ext-value`).
///
/// Narrower than [`url_encode`] on purpose: RFC 8187 defines `attr-char`, which
/// includes `.`, `-`, `_` and `~`, and those must stay literal. Encoding a dot
/// as `%2E` is technically legal but produces `photo%2Ejpg` in the header,
/// which older download managers have been known to save verbatim, extension
/// and all.
pub(crate) fn rfc8187_encode(value: &str) -> String {
    const ATTR_CHAR_EXTRAS: &[u8] = b"!#$&+-.^_`|~";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || ATTR_CHAR_EXTRAS.contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Path form for `Content-Disposition` and other header values: strip anything
/// that could terminate the quoted-string or inject a header.
pub(crate) fn header_safe_ascii(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' {
                c
            } else if c == ' ' {
                ' '
            } else {
                '_'
            }
        })
        .collect()
}
