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

pub(crate) fn index_html() -> String {
    let raw = disk_override("index.html").unwrap_or_else(|| INDEX_HTML.to_string());
    raw.replace("__REV__", asset_rev())
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

pub(crate) const MANIFEST_JSON: &str = r##"{
  "name": "LAN Share",
  "short_name": "LAN Share",
  "start_url": "/",
  "display": "standalone",
  "background_color": "#0b0b0d",
  "theme_color": "#0b0b0d",
  "icons": [
    { "src": "/assets/icon.svg", "sizes": "any", "type": "image/svg+xml" }
  ]
}"##;

pub(crate) const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#3b82f6" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.55a11 11 0 0 1 14.08 0"/><path d="M1.42 9a16 16 0 0 1 21.16 0"/><path d="M8.53 16.11a6 6 0 0 1 6.95 0"/><line x1="12" y1="20" x2="12.01" y2="20"/></svg>"##;
