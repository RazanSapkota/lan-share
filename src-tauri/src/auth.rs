//! PIN, session and token handling, plus the axum extractor that makes the
//! auth check unforgettable.
//!
//! **Why a cookie and not a header.** `<video src>`, `<img src>`, `<audio src>`
//! and `<a download>` cannot carry an `Authorization` header. The alternatives
//! were: a Service Worker rewriting requests (impossible -- Service Workers
//! require a secure context, and `http://192.168.1.5:8080` is not one), or a
//! token in the query string (works, but it then lands in every media URL, so
//! pasting one video link into a group chat hands out the whole session, and it
//! leaks through `Referer`). A cookie is attached automatically to every
//! same-origin subresource and keeps URLs clean.
//!
//! `Authorization: Bearer` is *additionally* accepted so every route stays
//! curl-testable and a future native client needs no server change.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::{header, request::Parts, StatusCode},
};
use subtle::ConstantTimeEq;

use crate::{
    models::{
        AttemptState, ServerCtx, Session, SessionScope, COOKIE_NAME, CSRF_HEADER,
    },
    utils::now_ms,
};

// ---------------------------------------------------------------------------
// Random material
// ---------------------------------------------------------------------------

/// Crockford-ish base32: no I/L/O/U, so a token read aloud or typed by hand
/// cannot be confused between 1/I/l or 0/O.
const TOKEN_ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    // The OS CSPRNG (BCryptGenRandom on Windows). A failure here means the
    // system entropy source is broken; there is no safe weaker fallback, so
    // mixing in time-based material would only disguise the problem.
    getrandom::fill(&mut buf).expect("OS random number generator unavailable");
    buf
}

fn encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| TOKEN_ALPHABET[(*b as usize) % TOKEN_ALPHABET.len()] as char)
        .collect()
}

/// 26 chars over a 32-symbol alphabet: 130 bits. Used for share links and
/// session ids alike.
pub(crate) fn random_token() -> String {
    encode(&random_bytes(26))
}

/// Shorter, still 60 bits -- ids are not secrets, they only need to not collide.
pub(crate) fn random_id() -> String {
    encode(&random_bytes(12)).to_lowercase()
}

/// A 6-digit PIN, uniformly distributed.
///
/// The modulo is taken over a `u32` drawn from 4 bytes, so the bias is 1 part in
/// ~4295 -- irrelevant against a 5-attempt lockout, and avoiding it entirely
/// would mean a rejection loop for no practical gain.
pub(crate) fn random_pin() -> String {
    let bytes = random_bytes(4);
    let value = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    format!("{:06}", value % 1_000_000)
}

/// Constant-time string comparison.
///
/// Length is compared first and in the clear -- that is unavoidable and
/// harmless, since token and PIN lengths are fixed and public.
pub(crate) fn ct_eq_str(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

// ---------------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------------

pub(crate) fn parse_cookie(header_value: &str, name: &str) -> Option<String> {
    header_value.split(';').find_map(|pair| {
        let pair = pair.trim();
        let (key, value) = pair.split_once('=')?;
        if key.trim() == name {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

/// No `Secure`: we are on plaintext HTTP by necessity -- no CA issues a
/// certificate for an RFC1918 address, and a self-signed cert produces a
/// full-page warning that defeats the "just scan the QR" flow.
///
/// `HttpOnly` so a filename-driven XSS in the gallery cannot read the session.
/// `SameSite=Lax` still permits `<a download>` top-level navigations but blocks
/// cross-site POSTs, which is the CSRF defense for `/api/upload`.
pub(crate) fn session_cookie(token: &str, ttl_hours: u32) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        ttl_hours as u64 * 3600
    )
}

pub(crate) fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

// ---------------------------------------------------------------------------
// Session store
// ---------------------------------------------------------------------------

pub(crate) fn create_session(
    sessions: &Arc<Mutex<HashMap<String, Session>>>,
    scope: SessionScope,
    client_ip: &str,
    user_agent: &str,
) -> String {
    let token = random_token();
    let now = now_ms();
    if let Ok(mut guard) = sessions.lock() {
        guard.insert(
            token.clone(),
            Session {
                scope,
                created_ms: now,
                last_seen_ms: now,
                client_ip: client_ip.to_string(),
                user_agent: user_agent.to_string(),
            },
        );
    }
    token
}

/// Look up a session, refresh its `last_seen`, and drop it if it has expired.
///
/// Expiry is checked against `created_ms`, not `last_seen_ms`: a sliding window
/// means an idle-but-open phone browser holds a session open indefinitely.
fn touch_session(
    sessions: &Arc<Mutex<HashMap<String, Session>>>,
    token: &str,
    ttl_hours: u32,
) -> Option<Session> {
    let mut guard = sessions.lock().ok()?;
    let now = now_ms();
    let ttl_ms = ttl_hours as u64 * 3600 * 1000;

    let session = guard.get_mut(token)?;
    if now.saturating_sub(session.created_ms) > ttl_ms {
        guard.remove(token);
        return None;
    }
    session.last_seen_ms = now;
    Some(session.clone())
}

/// Drop every expired session. Called opportunistically from `/api/session`
/// rather than on a timer -- there is no background task to host one, and the
/// map is small enough that a full sweep is trivial.
pub(crate) fn sweep_sessions(
    sessions: &Arc<Mutex<HashMap<String, Session>>>,
    ttl_hours: u32,
) {
    let Ok(mut guard) = sessions.lock() else { return };
    let now = now_ms();
    let ttl_ms = ttl_hours as u64 * 3600 * 1000;
    guard.retain(|_, s| now.saturating_sub(s.created_ms) <= ttl_ms);
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

pub(crate) enum PinCheck {
    Ok,
    Wrong { attempts_left: u32 },
    Locked { retry_after_ms: u64 },
}

/// Check a PIN and update the per-IP backoff.
///
/// Keyed by IP rather than by session, because an attacker guessing a 6-digit
/// PIN has no session yet. On a LAN the IP is a meaningful identity.
pub(crate) fn check_pin(
    attempts: &Arc<Mutex<HashMap<IpAddr, AttemptState>>>,
    ip: IpAddr,
    supplied: &str,
    expected: &str,
    max_attempts: u32,
    lockout_seconds: u32,
) -> PinCheck {
    let now = now_ms();
    let Ok(mut guard) = attempts.lock() else {
        // A poisoned lock must not become an auth bypass.
        return PinCheck::Wrong { attempts_left: 0 };
    };
    let entry = guard.entry(ip).or_default();

    if entry.locked_until_ms > now {
        return PinCheck::Locked {
            retry_after_ms: entry.locked_until_ms - now,
        };
    }

    if ct_eq_str(supplied, expected) && !expected.is_empty() {
        entry.failures = 0;
        entry.locked_until_ms = 0;
        return PinCheck::Ok;
    }

    entry.failures += 1;
    if entry.failures >= max_attempts {
        entry.failures = 0;
        entry.locked_until_ms = now + lockout_seconds as u64 * 1000;
        return PinCheck::Locked {
            retry_after_ms: lockout_seconds as u64 * 1000,
        };
    }
    PinCheck::Wrong {
        attempts_left: max_attempts.saturating_sub(entry.failures),
    }
}

// ---------------------------------------------------------------------------
// Request-scoped helpers
// ---------------------------------------------------------------------------

pub(crate) fn client_ip(parts: &Parts) -> IpAddr {
    // No `X-Forwarded-For` handling on purpose: this server is meant to be
    // reached directly on a LAN, and trusting that header from an untrusted
    // client would let anyone spoof their way past the PIN rate limiter.
    parts
        .extensions
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
        .unwrap_or(IpAddr::from([0, 0, 0, 0]))
}

pub(crate) fn user_agent(parts: &Parts) -> String {
    parts
        .headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

/// Session token from the cookie, or from `Authorization: Bearer`.
pub(crate) fn extract_token(parts: &Parts) -> Option<String> {
    if let Some(cookie) = parts
        .headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(token) = parse_cookie(cookie, COOKIE_NAME) {
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

type Rejection = (StatusCode, [(header::HeaderName, String); 1], String);

fn unauthorized() -> Rejection {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::CONTENT_TYPE,
            "application/json; charset=utf-8".to_string(),
        )],
        r#"{"error":"unauthorized"}"#.to_string(),
    )
}

/// True when the request carries our CSRF header. `SameSite=Lax` already blocks
/// cross-site POSTs; this is the second lock on the same door.
pub(crate) fn has_csrf_header(parts: &Parts) -> bool {
    parts.headers.get(CSRF_HEADER).is_some()
}

/// Resolve the session for a request without failing the request.
pub(crate) fn lookup(ctx: &ServerCtx, parts: &Parts) -> Option<(String, Session)> {
    let ttl = ctx
        .settings
        .read()
        .map(|s| s.session_ttl_hours)
        .unwrap_or(12);
    let token = extract_token(parts)?;
    let session = touch_session(&ctx.sessions, &token, ttl)?;
    Some((token, session))
}

// ---------------------------------------------------------------------------
// The extractor
// ---------------------------------------------------------------------------

/// Any handler that takes this is authenticated **by construction** -- there is
/// no way to forget the check, and no code path where a handler runs without it.
pub(crate) struct AuthedSession {
    pub(crate) session: Session,
    pub(crate) client_ip: IpAddr,
    pub(crate) user_agent: String,
}

impl AuthedSession {
    /// Whether this session may see `share_id`.
    pub(crate) fn allows_share(&self, share_id: &str) -> bool {
        match &self.session.scope {
            // A peer is admitted only when peer browsing is enabled (checked
            // when the scope is minted), and then sees what a PIN holder sees.
            SessionScope::Full | SessionScope::Peer { .. } => true,
            SessionScope::Share { share_id: id, .. } => id == share_id,
        }
    }

}

impl FromRequestParts<ServerCtx> for AuthedSession {
    type Rejection = (StatusCode, [(header::HeaderName, String); 1], String);

    async fn from_request_parts(
        parts: &mut Parts,
        ctx: &ServerCtx,
    ) -> Result<Self, Self::Rejection> {
        let ip = client_ip(parts);
        let ua = user_agent(parts);

        // --- peer bearer token, checked FIRST ---
        //
        // This must run before the PIN-disabled branch below. That branch
        // admits ANY request as `Full`, so a blocked device would sail straight
        // past its own block the moment the user turned the PIN off -- a
        // failure with no symptom until someone looks. `peer_from_headers`
        // returns None for a blocked peer, and the `is_bearer` guard turns that
        // None into a refusal here rather than a fall-through.
        // A bearer token that IS a known peer is answered here and never falls
        // through. A bearer token that is not a peer at all drops to the
        // session path below, which keeps the whole route table curl-testable
        // with a session token.
        if let Some(peer) = crate::peers::peer_by_bearer_any(ctx, &parts.headers) {
            let browse_ok = ctx
                .settings
                .read()
                .map(|s| s.peer_browse_enabled)
                .unwrap_or(false);
            if peer.blocked || !browse_ok {
                return Err(unauthorized());
            }
            let now = now_ms();
            let label = format!("peer:{}", peer.name);
            return Ok(AuthedSession {
                session: Session {
                    scope: SessionScope::Peer {
                        peer_id: peer.device_id.clone(),
                    },
                    created_ms: now,
                    last_seen_ms: now,
                    client_ip: ip.to_string(),
                    // Named, so the activity log says which device browsed
                    // rather than showing a blank user agent.
                    user_agent: label.clone(),
                },
                client_ip: ip,
                user_agent: label,
            });
        }

        if let Some((_token, session)) = lookup(ctx, parts) {
            return Ok(AuthedSession {
                session,
                client_ip: ip,
                user_agent: ua,
            });
        }

        // PIN disabled: admit the request rather than 401ing. The host
        // explicitly chose an open LAN share, so making the client round-trip
        // through /api/auth with no PIN to supply would be pure ceremony.
        //
        // The session is NOT stored here. An extractor cannot attach a
        // Set-Cookie to the response, so storing one would mint a fresh
        // unreachable entry on every single request and grow the map without
        // bound. `/api/session` mints the real, cookie-backed session; this is
        // just an in-memory pass for the current request.
        let pin_enabled = ctx.settings.read().map(|s| s.pin_enabled).unwrap_or(true);
        if !pin_enabled {
            let now = now_ms();
            return Ok(AuthedSession {
                session: Session {
                    scope: SessionScope::Full,
                    created_ms: now,
                    last_seen_ms: now,
                    client_ip: ip.to_string(),
                    user_agent: ua.clone(),
                },
                client_ip: ip,
                user_agent: ua,
            });
        }

        Err(unauthorized())
    }
}
