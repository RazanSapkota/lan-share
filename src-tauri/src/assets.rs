//! The receiver web UI, compiled into the binary.
//!
//! `include_str!` rather than a sidecar directory, so the app ships as one
//! executable. In debug builds the files are re-read from disk on every request
//! so the receiver UI can be iterated without recompiling.

#[cfg(not(debug_assertions))]
use std::sync::OnceLock;

use crate::{models::APP_VERSION, utils::sha256_hex};

pub(crate) const INDEX_HTML: &str = include_str!("../web/index.html");
pub(crate) const APP_JS: &str = include_str!("../web/app.js");
pub(crate) const STYLES_CSS: &str = include_str!("../web/styles.css");

/// Cache-busting revision. Tied to the package version, so a release always
/// invalidates cached assets without needing a build script.
pub(crate) fn asset_rev() -> &'static str {
    APP_VERSION
}

/// In debug builds, prefer the on-disk copy so edits show up on reload.
/// `CARGO_MANIFEST_DIR` is a compile-time constant, so this costs nothing in
/// release where the branch is compiled out.
#[cfg(debug_assertions)]
fn disk_override(rel: &str) -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("web")
        .join(rel);
    std::fs::read_to_string(path).ok()
}

#[cfg(not(debug_assertions))]
fn disk_override(_rel: &str) -> Option<String> {
    None
}

/// The shell, with the host's name substituted into `<title>`.
///
/// Templated here rather than set from JS so the tab is named before a single
/// byte of script runs -- and so a visitor who never gets past the PIN screen
/// still sees which machine they are talking to. `clean_device_name` strips
/// controls and bidi overrides but not `< > & "`, so this escapes.
pub(crate) fn index_html(device_name: &str) -> String {
    let raw = disk_override("index.html").unwrap_or_else(|| INDEX_HTML.to_string());
    raw.replace("__REV__", asset_rev())
        .replace("__NAME__", &crate::utils::escape_html(device_name))
}

pub(crate) fn app_js() -> String {
    disk_override("app.js").unwrap_or_else(|| APP_JS.to_string())
}

pub(crate) fn styles_css() -> String {
    disk_override("styles.css").unwrap_or_else(|| STYLES_CSS.to_string())
}

/// Strong ETag over the asset body.
///
/// In release the body is a `include_str!` constant, so the hash is computed
/// once and memoized. In debug `disk_override` can change it between requests,
/// so it is recomputed every time -- otherwise an edit would never reach the
/// browser, which defeats the point of the override.
pub(crate) fn etag_for(kind: &str, body: &str) -> String {
    #[cfg(debug_assertions)]
    {
        let _ = kind;
        return format!("\"{}\"", &sha256_hex(body)[..16]);
    }

    #[cfg(not(debug_assertions))]
    {
        static JS: OnceLock<String> = OnceLock::new();
        static CSS: OnceLock<String> = OnceLock::new();
        let cell = if kind == "js" { &JS } else { &CSS };
        cell.get_or_init(|| format!("\"{}\"", &sha256_hex(body)[..16]))
            .clone()
    }
}

/// Content-Security-Policy for the receiver page.
///
/// `frame-ancestors 'self'` rather than `'none'`: the desktop app's Preview
/// page hosts this exact page in an iframe on loopback, which is the point --
/// you see what receivers see, through the real auth path.
pub(crate) const CSP: &str = "default-src 'self'; \
     img-src 'self' data:; \
     media-src 'self'; \
     script-src 'self'; \
     style-src 'self'; \
     font-src 'self'; \
     connect-src 'self'; \
     object-src 'none'; \
     base-uri 'none'; \
     form-action 'self'; \
     frame-ancestors 'self'";

/// Named after the host, not the product: a phone that adds two machines to its
/// home screen would otherwise get two identical "LAN Share" icons with nothing
/// to tell them apart. Built with `json!` rather than string substitution so the
/// name is escaped by the serializer.
pub(crate) fn manifest_json(device_name: &str) -> String {
    serde_json::json!({
        "name": device_name,
        "short_name": device_name,
        "start_url": "/",
        "display": "standalone",
        "background_color": "#0b0b0d",
        "theme_color": "#0b0b0d",
        "icons": [
            { "src": "/assets/icon-192.png", "sizes": "192x192", "type": "image/png" },
            { "src": "/assets/icon-512.png", "sizes": "512x512", "type": "image/png" }
        ]
    })
    .to_string()
}

/// The app mark, cut from `tools/icon-source.png` by `tools/make-icons.mjs`.
///
/// A hand-drawn SVG of a folder and two arcs used to be served beside these,
/// standing in for artwork it did not resemble -- the guest page showed the
/// glyph while the window showed the real icon. There is one mark now, and it
/// is a raster, so every place that needs it takes one of these.
///
/// 192 is the manifest's small icon, the guest page's tab icon and the mark on
/// the PIN screen; one file for the three of them, so it is fetched once.
pub(crate) const ICON_192_PNG: &[u8] = include_bytes!("../icons/icon-192.png");
pub(crate) const ICON_512_PNG: &[u8] = include_bytes!("../icons/icon-512.png");
/// 16 and 32 only -- a kilobyte, rather than the whole multi-size app icon.
pub(crate) const FAVICON_ICO: &[u8] = include_bytes!("../icons/favicon.ico");
