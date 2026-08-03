//! Every axum handler. Thin by design: extract, delegate to `shares` / `media`
//! / `auth`, build a response.

use std::io::Write as _;

use axum::{
    body::Body,
    extract::{ConnectInfo, Multipart, Path as AxPath, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower::ServiceExt as _;
use tower_http::services::ServeFile;

use crate::{
    activity::CountingBody,
    assets, auth,
    auth::AuthedSession,
    media,
    models::{
        ActivityEntry, ResolvedShare, ServerCtx, ServerSettings, SessionScope, APP_VERSION,
    },
    shares::{self, PathReject},
    utils::{self, now_ms},
    zipstream::ZipStream,
};

// ---------------------------------------------------------------------------
// Small response helpers
// ---------------------------------------------------------------------------

fn json_error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "error": code }))).into_response()
}

fn reject_to_response(reject: &PathReject) -> Response {
    let status =
        StatusCode::from_u16(reject.status_code()).unwrap_or(StatusCode::BAD_REQUEST);
    (
        status,
        Json(json!({ "error": "path_rejected", "detail": reject.message() })),
    )
        .into_response()
}

fn text_asset(body: String, content_type: &'static str, etag: String, headers: &HeaderMap) -> Response {
    if let Some(inm) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
        if inm.split(',').any(|t| t.trim() == etag) {
            return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
        }
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::ETAG, etag),
            // Short max-age plus a strong ETag: the revalidation is one cheap
            // 304 on a LAN, and a stale asset after an app update is worse.
            (header::CACHE_CONTROL, "public, max-age=300".to_string()),
        ],
        body,
    )
        .into_response()
}

fn settings_of(ctx: &ServerCtx) -> ServerSettings {
    ctx.settings
        .read()
        .map(|s| s.clone())
        .unwrap_or_default()
}

/// Resolve a share the session is allowed to see.
fn scoped_share(
    ctx: &ServerCtx,
    session: &AuthedSession,
    share_id: &str,
) -> Result<ResolvedShare, Response> {
    if !session.allows_share(share_id) {
        // 404 rather than 403: a share-scoped link must not be able to
        // enumerate which other share ids exist.
        return Err(json_error(StatusCode::NOT_FOUND, "not_found"));
    }
    let registry = ctx
        .shares
        .read()
        .map_err(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "state_poisoned"))?;
    match registry.by_id(share_id) {
        Some(share) if shares::is_servable(share) => Ok(share.clone()),
        _ => Err(json_error(StatusCode::NOT_FOUND, "not_found")),
    }
}

// ---------------------------------------------------------------------------
// Host guard
// ---------------------------------------------------------------------------

/// DNS-rebinding defense.
///
/// Without it, a page on an attacker's domain can re-resolve that domain to the
/// victim's LAN IP and then read this server's responses as same-origin. We
/// only accept a `Host` whose hostname is an IP literal or `localhost` -- which
/// is every legitimate way to reach a LAN server, and no domain name.
pub(crate) async fn host_guard(
    State(ctx): State<ServerCtx>,
    request: Request,
    next: Next,
) -> Response {
    let strict = ctx
        .settings
        .read()
        .map(|s| s.strict_host_check)
        .unwrap_or(true);
    if !strict {
        return next.run(request).await;
    }

    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Strip the port. An IPv6 literal is bracketed, so split on the last colon
    // only when there is no closing bracket after it.
    let hostname = match host.rfind(':') {
        Some(idx) if !host[idx..].contains(']') => &host[..idx],
        _ => host,
    };
    let hostname = hostname.trim_matches(['[', ']']);

    let ok = hostname.is_empty()
        || hostname.eq_ignore_ascii_case("localhost")
        || hostname.parse::<std::net::IpAddr>().is_ok();

    if ok {
        next.run(request).await
    } else {
        (
            StatusCode::MISDIRECTED_REQUEST,
            Json(json!({
                "error": "bad_host",
                "detail": "Reach this server by IP address, not by hostname."
            })),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------
// Shell + static assets
// ---------------------------------------------------------------------------

pub(crate) async fn shell() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, assets::CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        assets::index_html(),
    )
        .into_response()
}

pub(crate) async fn asset_js(headers: HeaderMap) -> Response {
    let body = assets::app_js();
    let etag = assets::etag_for("js", &body);
    text_asset(body, "application/javascript; charset=utf-8", etag, &headers)
}

pub(crate) async fn asset_css(headers: HeaderMap) -> Response {
    let body = assets::styles_css();
    let etag = assets::etag_for("css", &body);
    text_asset(body, "text/css; charset=utf-8", etag, &headers)
}

pub(crate) async fn asset_icon() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        assets::ICON_SVG,
    )
        .into_response()
}

/// The 16/32 `.ico`, for browsers that ignore an SVG favicon.
///
/// This used to answer `204` -- browsers ask for it unprompted, and a 404 per
/// page load is noise -- but an empty answer means a blank tab icon on anything
/// that will not take the SVG, which is a poor showing for an app whose whole
/// job is to be opened in someone else's browser.
pub(crate) async fn favicon() -> Response {
    binary_asset(assets::FAVICON_ICO, "image/x-icon")
}

pub(crate) async fn asset_icon_192() -> Response {
    binary_asset(assets::ICON_192_PNG, "image/png")
}

pub(crate) async fn asset_icon_512() -> Response {
    binary_asset(assets::ICON_512_PNG, "image/png")
}

/// Baked into the binary and only ever changing with it, so a long cache is
/// safe -- the same reasoning `asset_icon` uses.
fn binary_asset(body: &'static [u8], mime: &'static str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        body,
    )
        .into_response()
}

pub(crate) async fn manifest() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/manifest+json")],
        assets::MANIFEST_JSON,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Session / auth
// ---------------------------------------------------------------------------

pub(crate) async fn ping(State(ctx): State<ServerCtx>) -> Response {
    let settings = settings_of(&ctx);
    // Deliberately leaks nothing beyond liveness and whether a PIN is needed.
    Json(json!({
        "app": "lanshare",
        "version": APP_VERSION,
        "name": settings.server_name,
        "pinRequired": settings.pin_enabled,
    }))
    .into_response()
}

pub(crate) async fn session_info(State(ctx): State<ServerCtx>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let settings = settings_of(&ctx);
    auth::sweep_sessions(&ctx.sessions, settings.session_ttl_hours);
    auth::sweep_play_tokens(&ctx.play_tokens);

    let ip = auth::client_ip(&parts);
    let ua = auth::user_agent(&parts);

    let existing = auth::lookup(&ctx, &parts);

    // PIN disabled: mint a Full session here so the client never sees the PIN
    // screen flash before learning it isn't needed.
    let (session, set_cookie) = match existing {
        Some((_, session)) => (Some(session), None),
        None if !settings.pin_enabled => {
            let token = auth::create_session(
                &ctx.sessions,
                SessionScope::Full,
                &ip.to_string(),
                &ua,
            );
            let cookie = auth::session_cookie(&token, settings.session_ttl_hours);
            let session = ctx
                .sessions
                .lock()
                .ok()
                .and_then(|s| s.get(&token).cloned());
            (session, Some(cookie))
        }
        None => (None, None),
    };

    let (share_id, share_name, can_upload) = match session.as_ref().and_then(|s| s.share_id()) {
        Some(id) => {
            let registry = ctx.shares.read().ok();
            let found = registry.as_ref().and_then(|r| r.by_id(id).cloned());
            match found {
                Some(share) => (
                    Some(share.cfg.id.clone()),
                    Some(share.cfg.name.clone()),
                    shares::can_upload_to(&share, &settings),
                ),
                None => (Some(id.to_string()), None, false),
            }
        }
        None => (None, None, settings.uploads_enabled),
    };

    let body = json!({
        "authenticated": session.is_some(),
        "scope": session.as_ref().map(|s| s.scope_name()).unwrap_or("none"),
        "shareId": share_id,
        "shareName": share_name,
        "pinRequired": settings.pin_enabled,
        "canUpload": can_upload,
        "thumbnails": settings.thumbnails_enabled,
        "allowZip": settings.allow_folder_zip,
        "serverName": settings.server_name,
        "defaultView": settings.default_view_mode,
        "defaultSort": settings.default_sort,
        "version": APP_VERSION,
    });

    match set_cookie {
        Some(cookie) => (
            StatusCode::OK,
            [(header::SET_COOKIE, cookie)],
            Json(body),
        )
            .into_response(),
        None => Json(body).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct AuthBody {
    #[serde(default)]
    pin: String,
}

pub(crate) async fn auth_pin(
    State(ctx): State<ServerCtx>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<AuthBody>,
) -> Response {
    let settings = settings_of(&ctx);
    let ip = peer.ip();
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect::<String>();

    if !settings.pin_enabled {
        let token =
            auth::create_session(&ctx.sessions, SessionScope::Full, &ip.to_string(), &ua);
        return (
            StatusCode::OK,
            [(
                header::SET_COOKIE,
                auth::session_cookie(&token, settings.session_ttl_hours),
            )],
            Json(json!({ "scope": "full" })),
        )
            .into_response();
    }

    match auth::check_pin(
        &ctx.pin_attempts,
        ip,
        body.pin.trim(),
        &settings.pin,
        settings.max_pin_attempts,
        settings.lockout_seconds,
    ) {
        auth::PinCheck::Ok => {
            let token =
                auth::create_session(&ctx.sessions, SessionScope::Full, &ip.to_string(), &ua);
            ctx.log_event("auth", "ok", &ip.to_string(), &ua, None, None, None);
            (
                StatusCode::OK,
                [(
                    header::SET_COOKIE,
                    auth::session_cookie(&token, settings.session_ttl_hours),
                )],
                Json(json!({ "scope": "full" })),
            )
                .into_response()
        }
        auth::PinCheck::Wrong { attempts_left } => {
            ctx.log_event(
                "auth_failed",
                "error",
                &ip.to_string(),
                &ua,
                None,
                None,
                Some(format!("{attempts_left} attempts left")),
            );
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid_pin", "attemptsLeft": attempts_left })),
            )
                .into_response()
        }
        auth::PinCheck::Locked { retry_after_ms } => {
            ctx.log_event(
                "auth_failed",
                "error",
                &ip.to_string(),
                &ua,
                None,
                None,
                Some("rate limited".to_string()),
            );
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(
                    header::RETRY_AFTER,
                    ((retry_after_ms / 1000).max(1)).to_string(),
                )],
                Json(json!({ "error": "rate_limited", "retryAfterMs": retry_after_ms })),
            )
                .into_response()
        }
    }
}

pub(crate) async fn logout(State(ctx): State<ServerCtx>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !auth::has_csrf_header(&parts) {
        return json_error(StatusCode::FORBIDDEN, "csrf");
    }
    if let Some(token) = auth::extract_token(&parts) {
        if let Ok(mut sessions) = ctx.sessions.lock() {
            sessions.remove(&token);
        }
    }
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, auth::clear_cookie())],
    )
        .into_response()
}

/// Per-share secret link. Sets a share-scoped cookie and redirects into the
/// client's query-string router.
pub(crate) async fn share_link(
    State(ctx): State<ServerCtx>,
    AxPath(token): AxPath<String>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let settings = settings_of(&ctx);
    let found = ctx
        .shares
        .read()
        .ok()
        .and_then(|r| r.by_token(&token).cloned());

    let Some(share) = found.filter(shares::is_servable) else {
        // 404, never 403: a 403 would confirm that the token space exists and
        // turn this into an oracle.
        return json_error(StatusCode::NOT_FOUND, "not_found");
    };

    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect::<String>();

    let session_token = auth::create_session(
        &ctx.sessions,
        SessionScope::Share {
            share_id: share.cfg.id.clone(),
            // Storing the token means regenerating it instantly invalidates
            // every session that entered through the old link.
            token: share.cfg.token.clone(),
        },
        &peer.ip().to_string(),
        &ua,
    );

    ctx.log_event(
        "auth",
        "ok",
        &peer.ip().to_string(),
        &ua,
        Some((&share.cfg.id, &share.cfg.name)),
        None,
        Some("share link".to_string()),
    );

    (
        [(
            header::SET_COOKIE,
            auth::session_cookie(&session_token, settings.session_ttl_hours),
        )],
        Redirect::to(&format!("/?s={}", utils::url_encode(&share.cfg.id))),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Browsing
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ShareSummary {
    id: String,
    name: String,
    is_inbox: bool,
    can_upload: bool,
    is_file: bool,
}

pub(crate) async fn list_shares(
    State(ctx): State<ServerCtx>,
    session: AuthedSession,
) -> Response {
    let settings = settings_of(&ctx);
    let Ok(registry) = ctx.shares.read() else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "state_poisoned");
    };

    let items: Vec<ShareSummary> = registry
        .shares
        .iter()
        .filter(|s| shares::is_servable(s))
        .filter(|s| session.allows_share(&s.cfg.id))
        .map(|s| ShareSummary {
            id: s.cfg.id.clone(),
            name: s.cfg.name.clone(),
            is_inbox: s.cfg.is_inbox,
            can_upload: shares::can_upload_to(s, &settings),
            is_file: s.cfg.is_file,
        })
        .collect();

    Json(items).into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListQuery {
    share: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    order: Option<String>,
}

pub(crate) async fn list_dir(
    State(ctx): State<ServerCtx>,
    session: AuthedSession,
    Query(q): Query<ListQuery>,
) -> Response {
    let settings = settings_of(&ctx);
    let share = match scoped_share(&ctx, &session, &q.share) {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let ascending = q.order.as_deref().map(|o| !o.eq_ignore_ascii_case("desc"));

    match shares::list_directory(&share, &q.path, &settings, q.sort.as_deref(), ascending) {
        Ok(listing) => {
            ctx.log_event(
                "list",
                "ok",
                &session.client_ip.to_string(),
                &session.user_agent,
                Some((&share.cfg.id, &share.cfg.name)),
                Some(listing.path.clone()),
                None,
            );
            Json(listing).into_response()
        }
        Err(reject) => {
            if reject == PathReject::Escapes || reject.status_code() == 400 {
                ctx.log_event(
                    "denied",
                    "error",
                    &session.client_ip.to_string(),
                    &session.user_agent,
                    Some((&share.cfg.id, &share.cfg.name)),
                    Some(q.path.clone()),
                    Some(reject.message()),
                );
            }
            reject_to_response(&reject)
        }
    }
}

// ---------------------------------------------------------------------------
// File serving
// ---------------------------------------------------------------------------

pub(crate) async fn serve_inline(
    State(ctx): State<ServerCtx>,
    session: AuthedSession,
    AxPath((share_id, rel)): AxPath<(String, String)>,
    request: Request,
) -> Response {
    serve_file(ctx, session, share_id, rel, request, false).await
}

pub(crate) async fn serve_attachment(
    State(ctx): State<ServerCtx>,
    session: AuthedSession,
    AxPath((share_id, rel)): AxPath<(String, String)>,
    request: Request,
) -> Response {
    serve_file(ctx, session, share_id, rel, request, true).await
}

/// Session-authenticated file serving. Resolves the share, then hands off to
/// `serve_resolved`, which is also where the play-token route lands.
async fn serve_file(
    ctx: ServerCtx,
    session: AuthedSession,
    share_id: String,
    rel: String,
    request: Request,
    attachment: bool,
) -> Response {
    let share = match scoped_share(&ctx, &session, &share_id) {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    serve_resolved(
        &ctx,
        &share,
        &rel,
        request,
        attachment,
        &session.client_ip.to_string(),
        &session.user_agent,
    )
    .await
}

/// The core streaming path.
///
/// `tower_http::services::ServeFile` does the genuinely hard part: `Range` ->
/// `206` + `Content-Range`, `If-Range`, `Last-Modified`/`If-Modified-Since`,
/// `416` on an unsatisfiable range, `HEAD`, and chunked streaming that never
/// buffers the file. Getting any of that wrong means iOS Safari silently
/// refuses to play the video, with no error anywhere -- and an external player
/// scrubbing a two-hour film leans on the same `Range` handling.
///
/// Takes the actor as plain strings rather than an `AuthedSession`, because a
/// play link has no session: the whole point of routing both through here is
/// that a player and a browser get byte-for-byte the same serving code.
async fn serve_resolved(
    ctx: &ServerCtx,
    share: &crate::models::ResolvedShare,
    rel: &str,
    request: Request,
    attachment: bool,
    client_ip: &str,
    actor: &str,
) -> Response {
    let rel = rel.to_string();
    let path = match shares::resolve_file(share, &rel) {
        Ok(p) => p,
        Err(reject) => {
            if reject == PathReject::Escapes || reject.status_code() == 400 {
                ctx.log_event(
                    "denied",
                    "error",
                    client_ip,
                    actor,
                    Some((&share.cfg.id, &share.cfg.name)),
                    Some(rel.clone()),
                    Some(reject.message()),
                );
            }
            return reject_to_response(&reject);
        }
    };

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());
    let total_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mime = media::mime_for(&path);

    // Correct MIME is mandatory, not cosmetic: iOS refuses to play a video
    // served as application/octet-stream.
    let service = ServeFile::new_with_mime(
        &path,
        &mime.parse().unwrap_or(mime::APPLICATION_OCTET_STREAM),
    );

    let response = match service.oneshot(request).await {
        Ok(resp) => resp,
        // A file another process holds exclusively (Premiere, Outlook .pst)
        // returns a sharing violation. 404 + an activity entry beats a 500.
        Err(_) => {
            ctx.log_event(
                "download",
                "error",
                client_ip,
                actor,
                Some((&share.cfg.id, &share.cfg.name)),
                Some(rel.clone()),
                Some("file is locked or unreadable".to_string()),
            );
            return json_error(StatusCode::NOT_FOUND, "not_found");
        }
    };

    let entry_id = ctx.log(ActivityEntry {
        at_ms: now_ms(),
        client_ip: client_ip.to_string(),
        user_agent: actor.to_string(),
        kind: if attachment { "download" } else { "view" }.to_string(),
        share_id: Some(share.cfg.id.clone()),
        share_name: Some(share.cfg.name.clone()),
        path: Some(rel.clone()),
        total_bytes,
        status: "started".to_string(),
        ..Default::default()
    });

    let (mut parts, body) = response.into_parts();

    let disposition = if attachment {
        // RFC 5987 `filename*` carries the real (possibly non-ASCII) name;
        // `filename` is the ASCII fallback for older clients.
        format!(
            "attachment; filename=\"{}\"; filename*=UTF-8''{}",
            utils::header_safe_ascii(&file_name),
            utils::rfc8187_encode(&file_name)
        )
    } else {
        "inline".to_string()
    };
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        parts.headers.insert(header::CONTENT_DISPOSITION, value);
    }
    parts.headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=0, must-revalidate"),
    );
    parts.headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );

    Response::from_parts(
        parts,
        Body::new(CountingBody::new(body, ctx.clone(), entry_id)),
    )
}

// ---------------------------------------------------------------------------
// Play links -- handing a file to an external player
// ---------------------------------------------------------------------------
//
// No browser decodes Matroska, and no codec can be installed into one to change
// that. The file is perfectly playable; the software looking at it is just the
// wrong software. So mint a link and let VLC (or mpv, or MX Player, or the TV)
// do what it already does well.
//
// The link cannot carry our cookie -- a player is not a browser and has no
// access to one -- so the credential rides in the path. `PlayToken` is the
// narrow shape that makes that acceptable: one file, six hours, and accepted
// here and nowhere else.

#[derive(Debug, Deserialize)]
pub(crate) struct PlayLinkBody {
    share: String,
    #[serde(default)]
    path: String,
}

#[derive(Debug, Serialize)]
struct PlayLinkResponse {
    token: String,
    name: String,
    expires_ms: u64,
}

/// Mint a play link for a file this session can already read.
///
/// The share and path go through exactly the checks `serve_file` applies, so
/// there is no file reachable through a play link that was not already
/// reachable through the session that asked for it.
pub(crate) async fn play_link(
    State(ctx): State<ServerCtx>,
    session: AuthedSession,
    headers: HeaderMap,
    Json(body): Json<PlayLinkBody>,
) -> Response {
    if !headers.contains_key(crate::models::CSRF_HEADER) {
        return json_error(StatusCode::FORBIDDEN, "csrf");
    }

    let share = match scoped_share(&ctx, &session, &body.share) {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let path = match shares::resolve_file(&share, &body.path) {
        Ok(p) => p,
        Err(reject) => {
            if reject == PathReject::Escapes || reject.status_code() == 400 {
                ctx.log_event(
                    "denied",
                    "error",
                    &session.client_ip.to_string(),
                    &session.user_agent,
                    Some((&share.cfg.id, &share.cfg.name)),
                    Some(body.path.clone()),
                    Some(reject.message()),
                );
            }
            return reject_to_response(&reject);
        }
    };

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "video".to_string());

    let token = auth::create_play_token(
        &ctx.play_tokens,
        &share.cfg.id,
        &body.path,
        &file_name,
    );

    // Logged as its own event: handing a file to another application is worth
    // seeing in the Activity list, and it happens some time before the bytes
    // start moving -- or instead of it, if the player never opens.
    ctx.log_event(
        "play",
        "ok",
        &session.client_ip.to_string(),
        &session.user_agent,
        Some((&share.cfg.id, &share.cfg.name)),
        Some(body.path.clone()),
        Some("opened in an external player".to_string()),
    );

    Json(PlayLinkResponse {
        token,
        name: file_name,
        expires_ms: crate::models::PLAY_TOKEN_TTL_MS,
    })
    .into_response()
}

/// Resolve a play token to a live share. `None` covers expired, revoked, and a
/// share that has since been switched off or deleted.
fn play_target(ctx: &ServerCtx, token: &str) -> Option<(crate::models::PlayToken, crate::models::ResolvedShare)> {
    let play = auth::resolve_play_token(&ctx.play_tokens, token)?;
    let share = ctx
        .shares
        .read()
        .ok()?
        .by_id(&play.share_id)
        .filter(|s| s.cfg.enabled)
        .filter(|s| shares::is_servable(s))
        .cloned()?;
    Some((play, share))
}

pub(crate) async fn play_stream(
    State(ctx): State<ServerCtx>,
    AxPath(token): AxPath<String>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    request: Request,
) -> Response {
    play_stream_inner(ctx, token, peer, request).await
}

/// Same, with a trailing filename. Purely cosmetic -- players guess the
/// container from the extension they can see in the URL, and a name in the
/// address bar beats an opaque token. It is NEVER used to build a path: the
/// path comes from the token, so `.../play/TOKEN/../../secret.txt` still serves
/// the file the token names.
pub(crate) async fn play_stream_named(
    State(ctx): State<ServerCtx>,
    AxPath((token, _name)): AxPath<(String, String)>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    request: Request,
) -> Response {
    play_stream_inner(ctx, token, peer, request).await
}

async fn play_stream_inner(
    ctx: ServerCtx,
    token: String,
    peer: std::net::SocketAddr,
    request: Request,
) -> Response {
    // 404 rather than 401: an expired link is indistinguishable from one that
    // never existed, which is what stops this being a probe oracle.
    let Some((play, share)) = play_target(&ctx, &token) else {
        return json_error(StatusCode::NOT_FOUND, "not_found");
    };

    let ua = request
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("external player")
        .chars()
        .take(200)
        .collect::<String>();

    serve_resolved(
        &ctx,
        &share,
        &play.rel,
        request,
        false,
        &peer.ip().to_string(),
        &ua,
    )
    .await
}

/// A one-entry `.m3u`, which is the closest thing to a universal "open this in
/// whatever plays video" handoff: every desktop and mobile player registers for
/// it, and it needs no custom URL scheme and no app to be installed in advance.
pub(crate) async fn play_playlist(
    State(ctx): State<ServerCtx>,
    AxPath(token): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some((play, _share)) = play_target(&ctx, &token) else {
        return json_error(StatusCode::NOT_FOUND, "not_found");
    };

    // The host the client actually reached us on, already validated by
    // `host_guard`. Guessing a LAN address here instead would hand the player
    // an address that may not route from where it is standing.
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if host.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "no_host");
    }

    // `url_encode_segment`, not `url_encode`: the player picks its demuxer from
    // the extension it sees, and `movie%2Emkv` has none.
    let url = format!(
        "http://{host}/play/{}/{}",
        utils::url_encode_segment(&token),
        utils::url_encode_segment(&play.file_name)
    );
    // The title line is what the player shows; a newline in a filename would
    // otherwise forge a second playlist entry.
    let title = play.file_name.replace(['\r', '\n'], " ");
    let body = format!("#EXTM3U\n#EXTINF:-1,{title}\n{url}\n");

    let download_name = format!("{}.m3u", utils::header_safe_ascii(&play.file_name));
    // `attachment`, and not because of the usual reasons. Served inline,
    // Firefox would offer its "open with" dialog -- a real chooser, which is
    // tempting -- but desktop Chrome navigates any `audio/*` document into its
    // own media viewer, which cannot play a playlist. That trades a file in the
    // downloads bar for a broken player page, on the more common browser.
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "audio/x-mpegurl".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{download_name}\""),
            ),
            (
                header::CACHE_CONTROL,
                "private, max-age=0, must-revalidate".to_string(),
            ),
        ],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Thumbnails
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct ThumbQuery {
    share: String,
    #[serde(default)]
    path: String,
}

pub(crate) async fn thumb(
    State(ctx): State<ServerCtx>,
    session: AuthedSession,
    headers: HeaderMap,
    Query(q): Query<ThumbQuery>,
) -> Response {
    let settings = settings_of(&ctx);
    if !settings.thumbnails_enabled {
        return json_error(StatusCode::NOT_FOUND, "thumbnails_disabled");
    }

    let share = match scoped_share(&ctx, &session, &q.share) {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let path = match shares::resolve_file(&share, &q.path) {
        Ok(p) => p,
        Err(reject) => return reject_to_response(&reject),
    };

    let ext = utils::ext_of(&path.to_string_lossy());
    if !media::can_thumbnail(&ext) {
        return json_error(StatusCode::NOT_FOUND, "no_thumbnail");
    }

    let cache_dir = ctx.thumb_dir.clone();
    let edge = settings.thumb_max_edge;
    let quality = settings.thumb_quality;

    // The permit bounds concurrent decodes: a folder of 5,000 photos would
    // otherwise start 5,000 full-resolution decodes at once.
    let Ok(_permit) = ctx.thumb_permits.clone().acquire_owned().await else {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "busy");
    };

    // Decoding is CPU-bound and blocking; keeping it off the runtime workers
    // is what lets downloads keep streaming while thumbnails render.
    let generated = tokio::task::spawn_blocking(move || {
        media::thumbnail(&path, &cache_dir, edge, quality)
    })
    .await;

    let (bytes, key) = match generated {
        Ok(Ok(pair)) => pair,
        _ => return json_error(StatusCode::NOT_FOUND, "thumbnail_failed"),
    };

    let etag = format!("\"{}\"", &key[..16.min(key.len())]);
    if let Some(inm) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
        if inm.split(',').any(|t| t.trim() == etag) {
            return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
        }
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/jpeg".to_string()),
            (header::ETAG, etag),
            // The key already encodes mtime+size, so a changed file produces a
            // different URL-independent ETag and `immutable` is safe.
            (
                header::CACHE_CONTROL,
                "private, max-age=604800, immutable".to_string(),
            ),
        ],
        bytes,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Folder zip
// ---------------------------------------------------------------------------

/// `/zip/{share}` and `/zip/{share}/` -- the share root.
///
/// A `{*path}` wildcard does not match an empty segment, so without this the
/// root form fell through to the SPA fallback and the client silently received
/// the HTML shell saved as a `.zip`.
pub(crate) async fn serve_zip_root(
    state: State<ServerCtx>,
    session: AuthedSession,
    AxPath(share_id): AxPath<String>,
) -> Response {
    serve_zip(state, session, AxPath((share_id, String::new()))).await
}

pub(crate) async fn serve_zip(
    State(ctx): State<ServerCtx>,
    session: AuthedSession,
    AxPath((share_id, rel)): AxPath<(String, String)>,
) -> Response {
    let settings = settings_of(&ctx);
    if !settings.allow_folder_zip {
        return json_error(StatusCode::FORBIDDEN, "zip_disabled");
    }

    let share = match scoped_share(&ctx, &session, &share_id) {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let dir = match shares::resolve_dir(&share, &rel) {
        Ok(d) => d,
        Err(reject) => return reject_to_response(&reject),
    };

    let folder_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| share.cfg.name.clone());

    let entry_id = ctx.log(ActivityEntry {
        at_ms: now_ms(),
        client_ip: session.client_ip.to_string(),
        user_agent: session.user_agent.clone(),
        kind: "download".to_string(),
        share_id: Some(share.cfg.id.clone()),
        share_name: Some(share.cfg.name.clone()),
        path: Some(rel.clone()),
        status: "started".to_string(),
        detail: Some("zip".to_string()),
        ..Default::default()
    });

    // Bridge a blocking zip writer to an async body through a bounded channel.
    // Bounded, so a slow phone applies backpressure instead of letting the
    // writer race ahead and buffer the whole archive in memory.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);
    let root = share.root.clone();

    tokio::task::spawn_blocking(move || {
        let mut zip = ZipStream::new(ChannelWriter { tx });

        for entry in walkdir::WalkDir::new(&dir)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            // Every entry is re-checked: a junction inside the tree must not
            // pull files from outside the share into the archive.
            if !shares::contained_in(&root, path) {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&dir) else {
                continue;
            };
            // Forward slashes: the format mandates them, and a backslash in an
            // entry name is what makes "zip slip" extractors write outside the
            // target directory.
            let name = rel.to_string_lossy().replace('\\', "/");

            let Ok(meta) = entry.metadata() else { continue };
            let mtime_ms = meta
                .modified()
                .map(utils::system_time_ms)
                .unwrap_or_default();
            let Ok(mut file) = std::fs::File::open(path) else {
                continue;
            };
            // A write error means the client disconnected; stop rather than
            // walk the rest of the tree into a dead channel.
            if zip
                .write_file(&name, meta.len(), mtime_ms, &mut file)
                .is_err()
            {
                return;
            }
        }
        let _ = zip.finish();
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = Body::from_stream(stream);

    let disposition = format!(
        "attachment; filename=\"{}.zip\"; filename*=UTF-8''{}.zip",
        utils::header_safe_ascii(&folder_name),
        utils::rfc8187_encode(&folder_name)
    );

    let mut response = Response::new(Body::new(CountingBody::new(body, ctx, entry_id)));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    // No Content-Length: the archive size is not known until it is written.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `std::io::Write` that pushes into a bounded async channel.
struct ChannelWriter {
    tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // blocking_send is correct here: this runs on a spawn_blocking thread,
        // and blocking is exactly the backpressure we want.
        self.tx
            .blocking_send(Ok(bytes::Bytes::copy_from_slice(buf)))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client gone"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct UploadQuery {
    share: String,
    #[serde(default)]
    path: String,
}

pub(crate) async fn upload(
    State(ctx): State<ServerCtx>,
    session: AuthedSession,
    headers: HeaderMap,
    Query(q): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Response {
    if !headers.contains_key(crate::models::CSRF_HEADER) {
        return json_error(StatusCode::FORBIDDEN, "csrf");
    }

    let settings = settings_of(&ctx);
    let share = match scoped_share(&ctx, &session, &q.share) {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    if !shares::can_upload_to(&share, &settings) {
        return json_error(StatusCode::FORBIDDEN, "uploads_disabled");
    }

    let mut saved = Vec::new();
    let mut rejected = Vec::new();

    while let Ok(Some(mut field)) = multipart.next_field().await {
        let Some(raw_name) = field.file_name().map(|s| s.to_string()) else {
            continue;
        };

        // A multipart filename is fully attacker-controlled. Take only the last
        // segment and then run it through the same validator every other path
        // goes through -- "../../../evil.exe" must not be a valid upload target.
        let base = raw_name
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .to_string();

        if shares::check_segment(&base).is_err() {
            rejected.push(json!({ "name": raw_name, "reason": "invalid_name" }));
            continue;
        }

        let ext = utils::ext_of(&base);
        if !settings.upload_allowed_ext.is_empty()
            && !settings.upload_allowed_ext.iter().any(|e| e == &ext)
        {
            rejected.push(json!({ "name": base, "reason": "extension_not_allowed" }));
            continue;
        }

        let target = match shares::resolve_new_within(&share.root, &q.path, &base) {
            Ok(t) => t,
            Err(reject) => {
                ctx.log_event(
                    "denied",
                    "error",
                    &session.client_ip.to_string(),
                    &session.user_agent,
                    Some((&share.cfg.id, &share.cfg.name)),
                    Some(base.clone()),
                    Some(reject.message()),
                );
                rejected.push(json!({ "name": base, "reason": "rejected" }));
                continue;
            }
        };

        let Some(parent) = target.parent().map(|p| p.to_path_buf()) else {
            rejected.push(json!({ "name": base, "reason": "rejected" }));
            continue;
        };

        // NEVER overwrite: a receiver must not be able to clobber the host's
        // file by uploading one with the same name.
        let final_path = if settings.upload_dedupe_names {
            match utils::unique_destination(&parent, &base) {
                Some(p) => p,
                None => {
                    rejected.push(json!({ "name": base, "reason": "name_taken" }));
                    continue;
                }
            }
        } else if target.exists() {
            rejected.push(json!({ "name": base, "reason": "already_exists" }));
            continue;
        } else {
            target
        };

        let mut file = match std::fs::File::create(&final_path) {
            Ok(f) => f,
            Err(_) => {
                rejected.push(json!({ "name": base, "reason": "write_failed" }));
                continue;
            }
        };

        let mut written = 0u64;
        let mut failed = false;
        // Streamed chunk by chunk -- `bytes()` would buffer a 4 GB upload in
        // memory before the first byte reaches disk.
        while let Ok(Some(chunk)) = field.chunk().await {
            if file.write_all(&chunk).is_err() {
                failed = true;
                break;
            }
            written += chunk.len() as u64;
        }

        if failed {
            let _ = std::fs::remove_file(&final_path);
            rejected.push(json!({ "name": base, "reason": "write_failed" }));
            continue;
        }

        let saved_name = final_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(base);

        ctx.log_event(
            "upload",
            "ok",
            &session.client_ip.to_string(),
            &session.user_agent,
            Some((&share.cfg.id, &share.cfg.name)),
            Some(saved_name.clone()),
            Some(utils::format_bytes(written)),
        );
        saved.push(json!({ "name": saved_name, "size": written }));
    }

    Json(json!({ "ok": rejected.is_empty(), "saved": saved, "rejected": rejected }))
        .into_response()
}

// ---------------------------------------------------------------------------
// Fallback
// ---------------------------------------------------------------------------

/// Anything under a data prefix is a real 404; everything else serves the
/// shell, so a deep link like `/?s=x&p=y` survives a hard reload.
///
/// The prefix list matters: a malformed `/files/...` or `/zip/...` that fell
/// through to the shell would hand the client an HTML page under the filename
/// it asked for, which looks like a successful download and is not.
pub(crate) async fn fallback(uri: Uri) -> Response {
    const DATA_PREFIXES: &[&str] = &["/api/", "/files/", "/download/", "/zip/", "/assets/", "/s/"];
    let path = uri.path();
    if DATA_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return json_error(StatusCode::NOT_FOUND, "not_found");
    }
    shell().await
}
