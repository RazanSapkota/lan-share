//! Pairing, the peer registry, and every `/api/peer/*` handler.
//!
//! These handlers do not live in `routes.rs` because they are not shaped like
//! the ones there. Every handler in `routes.rs` is *extract → delegate →
//! respond*; these own a state machine, and filing them alongside would cost
//! the property that makes `routes.rs` readable.

use std::{net::IpAddr, path::PathBuf, time::Duration};

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{ConnectInfo, Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::{
    auth,
    models::{
        AppState, Offer, OfferState, OfferedFile, PairPromptView, PairState, Peer, PendingPair,
        ServerCtx, OFFER_TTL_MS, PAIR_LOCKOUT_SECONDS, PAIR_MAX_ATTEMPTS, PAIR_TTL_MS,
    },
    shares, transfer,
    utils::{format_bytes, now_ms},
};

// ---------------------------------------------------------------------------
// Pairing cryptography
// ---------------------------------------------------------------------------

/// Length-prefixed concatenation.
///
/// Without the prefix, `("ab","c")` and `("a","bc")` hash identically. A device
/// picks its own id and its own name, so that is a collision an attacker can
/// actually steer into, not a theoretical one.
fn absorb(hasher: &mut Sha256, parts: &[&[u8]]) {
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
}

/// The initiator's binding commitment to its nonce.
///
/// This is what makes numeric comparison worth anything. Without it, an
/// attacker sitting between A and B runs both halves of the exchange and picks
/// each of its nonces *after* seeing the other side's, so it can grind them
/// until both screens show the same six digits and the user's comparison
/// passes. Committing first removes the grinding window. Bluetooth SSP does
/// exactly this, for exactly this reason.
pub(crate) fn commit_of(nonce: &str) -> String {
    let mut hasher = Sha256::new();
    absorb(&mut hasher, &[b"lanshare-commit-v1", nonce.as_bytes()]);
    hex(&hasher.finalize())
}

/// The six digits shown on both screens.
///
/// Both sides can compute this only once the nonce is revealed, and the
/// commitment proves the revealed nonce is the one that was committed to.
pub(crate) fn pair_code(
    commit: &str,
    nonce_a: &str,
    nonce_b: &str,
    id_a: &str,
    id_b: &str,
) -> String {
    let mut hasher = Sha256::new();
    absorb(
        &mut hasher,
        &[
            b"lanshare-pair-v1",
            commit.as_bytes(),
            nonce_a.as_bytes(),
            nonce_b.as_bytes(),
            id_a.as_bytes(),
            id_b.as_bytes(),
        ],
    );
    let digest = hasher.finalize();
    // 8 bytes into a u64 before the modulo: the bias is 10^6/2^64, which is
    // nothing. Folding 4 bytes would bias the leading digit measurably.
    let n = u64::from_be_bytes(digest[..8].try_into().unwrap_or([0; 8]));
    format!("{:06}", n % 1_000_000)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The single response every peer route gives for "no".
///
/// Byte-identical whether the token is unknown, the peer is blocked, or
/// peering is off entirely. A distinguishable 403 would turn the status code
/// into an oracle for which tokens exist.
fn peer_404() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response()
}

fn peering_on(ctx: &ServerCtx) -> bool {
    ctx.settings
        .read()
        .map(|s| s.peering_enabled)
        .unwrap_or(false)
}

fn bearer_of(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
}

/// Resolve a bearer token to a peer **whether or not it is blocked**.
///
/// The extractor needs to tell "this is a blocked peer" (refuse) from "this is
/// not a peer token at all" (fall through to the session path). Collapsing
/// those two into one `None` is what would let a blocked device slip in
/// through the PIN-disabled branch.
pub(crate) fn peer_by_bearer_any(ctx: &ServerCtx, headers: &HeaderMap) -> Option<Peer> {
    if !peering_on(ctx) {
        return None;
    }
    let token = bearer_of(headers)?;
    let registry = ctx.peers.read().ok()?;
    registry.by_in_token(token).cloned()
}

/// Resolve a bearer token to a usable peer, refusing blocked ones.
///
/// Used by the peer routes here AND by the `AuthedSession` extractor, so the
/// two can never disagree about who is allowed in.
pub(crate) fn peer_from_headers(ctx: &ServerCtx, headers: &HeaderMap) -> Option<Peer> {
    peer_by_bearer_any(ctx, headers).filter(|p| !p.blocked)
}

/// Note that we heard from a peer, so the desktop can show "last seen" without
/// waiting for a beacon.
fn touch_peer(ctx: &ServerCtx, peer_id: &str, address: &str) {
    if let Ok(mut registry) = ctx.peers.write() {
        if let Some(p) = registry.peers.iter_mut().find(|p| p.device_id == peer_id) {
            p.last_seen_ms = now_ms();
            p.last_address = Some(address.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// Receive folder
// ---------------------------------------------------------------------------

/// Where files sent by a peer land. Config first, then the OS Downloads folder.
pub(crate) fn resolve_receive_dir(app: &tauri::AppHandle, state: &AppState) -> Result<PathBuf> {
    use tauri::Manager;

    let configured = crate::config::load_config_impl(app).receive_dir;
    let dir = match configured.filter(|p| !p.trim().is_empty()) {
        Some(path) => PathBuf::from(path),
        None => app
            .path()
            .download_dir()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("LAN Share"),
    };

    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create the receive folder {}", dir.display()))?;
    // Canonical, because it is the containment root for every inbound
    // filename -- the same treatment a share root gets.
    let dir = shares::canonical_root(&dir.to_string_lossy())?;

    if let Ok(mut slot) = state.receive_dir.lock() {
        *slot = Some(dir.clone());
    }
    Ok(dir)
}

// ---------------------------------------------------------------------------
// Housekeeping
// ---------------------------------------------------------------------------

/// Expire stale discovery rows, abandoned pair prompts and dead offers.
pub(crate) async fn sweep_loop(ctx: ServerCtx, mut shutdown: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                crate::discovery::sweep(&ctx);
                sweep_pairs(&ctx);
                sweep_offers(&ctx);
                transfer::sweep_transfers(&ctx);
            }
            _ = shutdown.wait_for(|stop| *stop) => break,
        }
    }
}

fn sweep_pairs(ctx: &ServerCtx) {
    if let Ok(mut pending) = ctx.pending_pairs.lock() {
        let now = now_ms();
        pending.retain(|_, p| p.expires_ms > now);
    }
}

fn sweep_offers(ctx: &ServerCtx) {
    if let Ok(mut offers) = ctx.offers.lock() {
        let now = now_ms();
        offers.retain(|_, o| now.saturating_sub(o.created_ms) <= OFFER_TTL_MS);
    }
}

// ---------------------------------------------------------------------------
// GET /api/peer/hello
// ---------------------------------------------------------------------------

/// Liveness plus identity, for the manual "add by IP" path.
///
/// Public, because a device you have not paired with yet must be able to learn
/// your name to show it in the pairing dialog. It leaks nothing a beacon on the
/// same subnet would not already have broadcast.
pub(crate) async fn hello(State(ctx): State<ServerCtx>) -> Response {
    if !peering_on(&ctx) {
        return peer_404();
    }
    let (name, port) = ctx
        .settings
        .read()
        .map(|s| (s.device_name.clone(), s.port))
        .unwrap_or_default();
    Json(json!({
        "app": "lanshare",
        "deviceId": ctx.device_id.as_str(),
        "name": name,
        "platform": crate::models::platform_name(),
        "port": port,
        "version": crate::models::APP_VERSION,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /api/peer/pair/request
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct PairRequestBody {
    #[serde(default)]
    pub(crate) device_id: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) platform: String,
    #[serde(default)]
    pub(crate) port: u16,
    /// H("lanshare-commit-v1" || nonce_a).
    #[serde(default)]
    pub(crate) commit: String,
    /// The token WE should present when talking to THEM.
    #[serde(default)]
    pub(crate) out_token: String,
}

#[derive(Debug, Serialize)]
struct PairRequestReply {
    pair_id: String,
    nonce_b: String,
    name: String,
    device_id: String,
}

pub(crate) async fn pair_request(
    State(ctx): State<ServerCtx>,
    ConnectInfo(peer_addr): ConnectInfo<std::net::SocketAddr>,
    Json(body): Json<PairRequestBody>,
) -> Response {
    if !peering_on(&ctx) {
        return peer_404();
    }

    let ip = peer_addr.ip();
    if let Some(retry_ms) = pair_locked(&ctx, ip) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                axum::http::header::RETRY_AFTER,
                ((retry_ms / 1000).max(1)).to_string(),
            )],
            Json(json!({ "error": "rate_limited", "retryAfterMs": retry_ms })),
        )
            .into_response();
    }

    // Validate before storing: everything here is attacker-supplied.
    if body.device_id.is_empty()
        || body.device_id.len() > 64
        || !body.device_id.chars().all(|c| c.is_ascii_alphanumeric())
        || body.commit.len() != 64
        || !body.commit.chars().all(|c| c.is_ascii_hexdigit())
        || body.out_token.is_empty()
        || body.out_token.len() > 64
        || !body.out_token.chars().all(|c| c.is_ascii_alphanumeric())
    {
        pair_note_failure(&ctx, ip);
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "bad_request" })),
        )
            .into_response();
    }

    // Refuse to pair with ourselves. Reachable when both instances run on one
    // machine, and the resulting self-peer is confusing rather than harmful.
    if body.device_id == *ctx.device_id {
        pair_note_failure(&ctx, ip);
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "self_pairing" })),
        )
            .into_response();
    }

    // One live prompt per source IP, so nobody can paper the screen with
    // dialogs faster than a human can dismiss them.
    {
        let Ok(pending) = ctx.pending_pairs.lock() else {
            return peer_404();
        };
        let now = now_ms();
        if pending
            .values()
            .any(|p| p.from_ip == ip.to_string() && p.expires_ms > now)
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "pair_in_progress" })),
            )
                .into_response();
        }
    }

    let pair_id = auth::random_token();
    let nonce_b = auth::random_token();
    let now = now_ms();

    let entry = PendingPair {
        pair_id: pair_id.clone(),
        from_id: body.device_id.clone(),
        from_name: crate::config::clean_device_name(&body.name),
        from_platform: body
            .platform
            .chars()
            .filter(|c| c.is_ascii_lowercase() || *c == '-')
            .take(16)
            .collect(),
        from_ip: ip.to_string(),
        from_port: body.port,
        commit: body.commit,
        nonce_b: nonce_b.clone(),
        out_token: body.out_token,
        code: None,
        // Committed, not Waiting: we cannot compute the code yet, so the user
        // must be shown nothing at all until the reveal arrives.
        state: PairState::Committed,

        expires_ms: now + PAIR_TTL_MS,
        issued_in_token: None,
    };

    let name = ctx
        .settings
        .read()
        .map(|s| s.device_name.clone())
        .unwrap_or_default();

    if let Ok(mut pending) = ctx.pending_pairs.lock() {
        pending.insert(pair_id.clone(), entry);
    }
    pair_note_success(&ctx, ip);

    // The reply carries `nonce_b` but NOT the code -- the initiator derives
    // that itself. It also cannot carry anything derived from `nonce_a`,
    // because we have not seen it yet. That asymmetry is the whole point.
    Json(PairRequestReply {
        pair_id,
        nonce_b,
        name,
        device_id: ctx.device_id.as_str().to_string(),
    })
    .into_response()
}

/// Whether this address is currently locked out. Read-only.
fn pair_locked(ctx: &ServerCtx, ip: IpAddr) -> Option<u64> {
    let Ok(attempts) = ctx.pair_attempts.lock() else {
        return Some(1000);
    };
    let now = now_ms();
    attempts
        .get(&ip)
        .filter(|e| e.locked_until_ms > now)
        .map(|e| e.locked_until_ms - now)
}

/// Count a *refused* pair request, and lock the address out after too many.
///
/// Failures, not attempts -- the same rule `auth::check_pin` uses. Counting
/// every request instead would lock a user out of their own app for pairing
/// five devices in a row, which is a completely ordinary thing to do on a
/// first run.
fn pair_note_failure(ctx: &ServerCtx, ip: IpAddr) {
    let Ok(mut attempts) = ctx.pair_attempts.lock() else {
        return;
    };
    let now = now_ms();
    let entry = attempts.entry(ip).or_default();
    entry.failures += 1;
    if entry.failures >= PAIR_MAX_ATTEMPTS {
        entry.failures = 0;
        entry.locked_until_ms = now + PAIR_LOCKOUT_SECONDS as u64 * 1000;
    }
}

/// A well-formed request clears the counter: it is evidence of a real client,
/// not a guessing campaign. The one-prompt-per-IP rule below is what stops
/// this being a way to spam prompts.
fn pair_note_success(ctx: &ServerCtx, ip: IpAddr) {
    if let Ok(mut attempts) = ctx.pair_attempts.lock() {
        attempts.remove(&ip);
    }
}

// ---------------------------------------------------------------------------
// POST /api/peer/pair/reveal
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct PairRevealBody {
    #[serde(default)]
    pub(crate) pair_id: String,
    #[serde(default)]
    pub(crate) nonce: String,
}

/// The initiator reveals its nonce; we verify it against the commitment,
/// derive the code, and only now put a prompt in front of the user.
pub(crate) async fn pair_reveal(
    State(ctx): State<ServerCtx>,
    ConnectInfo(peer_addr): ConnectInfo<std::net::SocketAddr>,
    Json(body): Json<PairRevealBody>,
) -> Response {
    if !peering_on(&ctx) {
        return peer_404();
    }
    let ip = peer_addr.ip().to_string();

    let Ok(mut pending) = ctx.pending_pairs.lock() else {
        return peer_404();
    };
    let Some(entry) = pending.get_mut(&body.pair_id) else {
        return peer_404();
    };

    // Bound to the initiator's address, single-use, and time-limited. Any one
    // of those failing means this is not the exchange we started.
    if entry.from_ip != ip || entry.state != PairState::Committed || entry.expires_ms <= now_ms() {
        return peer_404();
    }

    if commit_of(&body.nonce) != entry.commit {
        // The revealed nonce is not the committed one: either a bug or someone
        // trying to steer the code. Drop the whole exchange.
        pending.remove(&body.pair_id);
        return peer_404();
    }

    let code = pair_code(
        &entry.commit,
        &body.nonce,
        &entry.nonce_b,
        &entry.from_id,
        ctx.device_id.as_str(),
    );
    entry.code = Some(code.clone());
    entry.state = PairState::Waiting;

    // The code is echoed so the initiator can assert both sides derived the
    // same value; it already knows it, so this reveals nothing new.
    Json(json!({ "code": code })).into_response()
}

// ---------------------------------------------------------------------------
// POST /api/peer/pair/poll
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct PairPollBody {
    #[serde(default)]
    pub(crate) pair_id: String,
}

pub(crate) async fn pair_poll(
    State(ctx): State<ServerCtx>,
    ConnectInfo(peer_addr): ConnectInfo<std::net::SocketAddr>,
    Json(body): Json<PairPollBody>,
) -> Response {
    if !peering_on(&ctx) {
        return peer_404();
    }
    let ip = peer_addr.ip().to_string();

    let Ok(mut pending) = ctx.pending_pairs.lock() else {
        return peer_404();
    };
    let Some(entry) = pending.get(&body.pair_id) else {
        return peer_404();
    };
    // A poll from anywhere else is refused WITHOUT consuming the entry, so a
    // guesser cannot burn a legitimate pairing it happened to hit.
    if entry.from_ip != ip {
        return peer_404();
    }
    if entry.expires_ms <= now_ms() {
        pending.remove(&body.pair_id);
        return peer_404();
    }

    match entry.state {
        PairState::Committed | PairState::Waiting => {
            Json(json!({ "state": "pending" })).into_response()
        }
        PairState::Declined => {
            let reply = Json(json!({ "state": "declined" })).into_response();
            pending.remove(&body.pair_id);
            reply
        }
        PairState::Accepted => {
            let token = entry.issued_in_token.clone().unwrap_or_default();
            let name = ctx
                .settings
                .read()
                .map(|s| s.device_name.clone())
                .unwrap_or_default();
            let reply = Json(json!({
                "state": "accepted",
                "token": token,
                "deviceId": ctx.device_id.as_str(),
                "name": name,
            }))
            .into_response();
            // Single-use: the token is handed over exactly once.
            pending.remove(&body.pair_id);
            reply
        }
    }
}

// ---------------------------------------------------------------------------
// Desktop-side pairing control (called from Tauri commands, not HTTP)
// ---------------------------------------------------------------------------

// `AppState` and `ServerCtx` hold the *same* Arcs under different names, so
// each of these operations is written once against the map and given two thin
// wrappers. Two copies of a state machine drift; two field accessors cannot.
type PendingMap = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, PendingPair>>>;
type OfferMap = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Offer>>>;

pub(crate) fn list_prompts(state: &AppState) -> Vec<PairPromptView> {
    list_prompts_in(&state.pending_pairs)
}

#[cfg(test)]
pub(crate) fn list_prompts_ctx(ctx: &ServerCtx) -> Vec<PairPromptView> {
    list_prompts_in(&ctx.pending_pairs)
}

fn list_prompts_in(pending: &PendingMap) -> Vec<PairPromptView> {
    let Ok(pending) = pending.lock() else {
        return Vec::new();
    };
    let now = now_ms();
    let mut out: Vec<PairPromptView> = pending
        .values()
        // Only `Waiting` has a code, and a prompt without a code is exactly
        // what the commitment step exists to prevent showing.
        .filter(|p| p.state == PairState::Waiting && p.expires_ms > now)
        .map(|p| PairPromptView {
            pair_id: p.pair_id.clone(),
            device_id: p.from_id.clone(),
            name: p.from_name.clone(),
            platform: p.from_platform.clone(),
            address: p.from_ip.clone(),
            code: p.code.clone().unwrap_or_default(),
            expires_ms: p.expires_ms,
        })
        .collect();
    out.sort_by_key(|p| p.expires_ms);
    out
}

/// Accept a pending pair: mint our inbound token and store the peer.
///
/// Returns the `Peer` so the caller can persist it in the config.
pub(crate) fn accept_prompt(state: &AppState, pair_id: &str) -> Result<Peer> {
    accept_prompt_in(&state.pending_pairs, pair_id)
}

#[cfg(test)]
pub(crate) fn accept_prompt_ctx(ctx: &ServerCtx, pair_id: &str) -> Result<Peer> {
    accept_prompt_in(&ctx.pending_pairs, pair_id)
}

fn accept_prompt_in(pending: &PendingMap, pair_id: &str) -> Result<Peer> {
    let Ok(mut pending) = pending.lock() else {
        return Err(anyhow!("pairing state poisoned"));
    };
    let entry = pending
        .get_mut(pair_id)
        .ok_or_else(|| anyhow!("that pairing request has expired"))?;
    if entry.state != PairState::Waiting {
        return Err(anyhow!("that pairing request is no longer waiting"));
    }
    if entry.expires_ms <= now_ms() {
        return Err(anyhow!("that pairing request has expired"));
    }

    let in_token = auth::random_token();
    entry.issued_in_token = Some(in_token.clone());
    entry.state = PairState::Accepted;

    Ok(Peer {
        device_id: entry.from_id.clone(),
        name: if entry.from_name.is_empty() {
            entry.from_id.clone()
        } else {
            entry.from_name.clone()
        },
        platform: entry.from_platform.clone(),
        in_token,
        out_token: entry.out_token.clone(),
        added_ms: now_ms(),
        last_seen_ms: now_ms(),
        last_address: Some(format!("{}:{}", entry.from_ip, entry.from_port)),
        auto_accept: false,
        blocked: false,
        note: None,
    })
}

pub(crate) fn decline_prompt(state: &AppState, pair_id: &str) -> Result<()> {
    decline_prompt_in(&state.pending_pairs, pair_id)
}

#[cfg(test)]
pub(crate) fn decline_prompt_ctx(ctx: &ServerCtx, pair_id: &str) -> Result<()> {
    decline_prompt_in(&ctx.pending_pairs, pair_id)
}

fn decline_prompt_in(pending: &PendingMap, pair_id: &str) -> Result<()> {
    let Ok(mut pending) = pending.lock() else {
        return Err(anyhow!("pairing state poisoned"));
    };
    let entry = pending
        .get_mut(pair_id)
        .ok_or_else(|| anyhow!("that pairing request has expired"))?;
    // Left in the map, not removed: the initiator's next poll must be able to
    // learn it was declined rather than time out wondering.
    entry.state = PairState::Declined;
    entry.issued_in_token = None;
    Ok(())
}

// ---------------------------------------------------------------------------
// POST /api/peer/offer
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct OfferBody {
    #[serde(default)]
    pub(crate) files: Vec<OfferedFile>,
}

pub(crate) async fn offer(
    State(ctx): State<ServerCtx>,
    ConnectInfo(peer_addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<OfferBody>,
) -> Response {
    let Some(peer) = peer_from_headers(&ctx, &headers) else {
        return peer_404();
    };
    touch_peer(&ctx, &peer.device_id, &peer_addr.ip().to_string());

    let (max_files, max_bytes) = ctx
        .settings
        .read()
        .map(|s| (s.max_offer_files, s.max_offer_bytes))
        .unwrap_or((500, u64::MAX));

    if body.files.is_empty() || body.files.len() > max_files {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "bad_offer" })),
        )
            .into_response();
    }

    // Validate EVERY filename before creating the offer. A rejected name fails
    // the whole batch, so the human is never shown a prompt whose contents
    // differ from what would actually land on disk.
    for file in &body.files {
        if shares::check_segment(&file.name).is_err() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "bad_filename" })),
            )
                .into_response();
        }
    }

    let total_bytes: u64 = body.files.iter().map(|f| f.size).sum();
    if total_bytes > max_bytes {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "too_large" })),
        )
            .into_response();
    }

    // One live offer per peer: a queue of prompts from one device is a way to
    // wear the user down into clicking Accept.
    {
        let Ok(offers) = ctx.offers.lock() else {
            return peer_404();
        };
        if offers
            .values()
            .any(|o| o.peer_id == peer.device_id && o.state == OfferState::Pending)
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "offer_in_progress" })),
            )
                .into_response();
        }
    }

    let offer_id = auth::random_token();
    let transfer_id = transfer::begin(
        &ctx,
        "in",
        &peer.device_id,
        &peer.name,
        body.files.len(),
        total_bytes,
    );

    // Auto-accept is a per-peer decision the user made once, in advance.
    let auto = peer.auto_accept;
    let state = if auto {
        OfferState::Accepted
    } else {
        OfferState::Pending
    };

    if let Ok(mut offers) = ctx.offers.lock() {
        offers.insert(
            offer_id.clone(),
            Offer {
                offer_id: offer_id.clone(),
                peer_id: peer.device_id.clone(),
                peer_name: peer.name.clone(),
                files: body.files.clone(),
                total_bytes,
                state,
                created_ms: now_ms(),
                received: Vec::new(),
                transfer_id,
            },
        );
    }

    ctx.log_event(
        "offer",
        if auto { "ok" } else { "started" },
        &peer_addr.ip().to_string(),
        &format!("peer:{}", peer.name),
        None,
        Some(format!(
            "{} file(s), {}",
            body.files.len(),
            format_bytes(total_bytes)
        )),
        None,
    );

    Json(json!({
        "offerId": offer_id,
        "state": if auto { "accepted" } else { "pending" },
    }))
    .into_response()
}

/// The sender polls this until the user answers.
pub(crate) async fn offer_status(
    State(ctx): State<ServerCtx>,
    headers: HeaderMap,
    AxPath(offer_id): AxPath<String>,
) -> Response {
    let Some(peer) = peer_from_headers(&ctx, &headers) else {
        return peer_404();
    };
    let Ok(offers) = ctx.offers.lock() else {
        return peer_404();
    };
    let Some(offer) = offers.get(&offer_id) else {
        return peer_404();
    };
    // A peer may only see its own offers.
    if offer.peer_id != peer.device_id {
        return peer_404();
    }
    Json(json!({
        "state": match offer.state {
            OfferState::Pending => "pending",
            OfferState::Accepted => "accepted",
            OfferState::Declined => "declined",
        }
    }))
    .into_response()
}

pub(crate) fn list_offers(state: &AppState) -> Vec<crate::models::OfferView> {
    list_offers_in(&state.offers)
}

#[cfg(test)]
pub(crate) fn list_offers_ctx(ctx: &ServerCtx) -> Vec<crate::models::OfferView> {
    list_offers_in(&ctx.offers)
}

fn list_offers_in(offers: &OfferMap) -> Vec<crate::models::OfferView> {
    let Ok(offers) = offers.lock() else {
        return Vec::new();
    };
    let mut out: Vec<crate::models::OfferView> = offers
        .values()
        .filter(|o| o.state == OfferState::Pending)
        .map(|o| {
            const PREVIEW: usize = 50;
            crate::models::OfferView {
                offer_id: o.offer_id.clone(),
                peer_id: o.peer_id.clone(),
                peer_name: o.peer_name.clone(),
                file_count: o.files.len(),
                total_bytes: o.total_bytes,
                total_bytes_text: format_bytes(o.total_bytes),
                // A 500-file offer must not ship 500 rows to the UI.
                files: o.files.iter().take(PREVIEW).cloned().collect(),
                truncated: o.files.len() > PREVIEW,
            }
        })
        .collect();
    out.sort_by_key(|o| o.offer_id.clone());
    out
}

pub(crate) fn set_offer_state(state: &AppState, offer_id: &str, accept: bool) -> Result<String> {
    let (peer_id, transfer_id) = set_offer_state_in(&state.offers, offer_id, accept)?;
    if !accept {
        transfer::finish(state, transfer_id, "declined", None);
    }
    Ok(peer_id)
}

#[cfg(test)]
pub(crate) fn set_offer_state_ctx(ctx: &ServerCtx, offer_id: &str, accept: bool) -> Result<String> {
    let (peer_id, transfer_id) = set_offer_state_in(&ctx.offers, offer_id, accept)?;
    if !accept {
        transfer::finish_ctx(ctx, transfer_id, "declined", None);
    }
    Ok(peer_id)
}

fn set_offer_state_in(offers: &OfferMap, offer_id: &str, accept: bool) -> Result<(String, u64)> {
    let Ok(mut offers) = offers.lock() else {
        return Err(anyhow!("offer state poisoned"));
    };
    let offer = offers
        .get_mut(offer_id)
        .ok_or_else(|| anyhow!("that transfer request has expired"))?;
    if offer.state != OfferState::Pending {
        return Err(anyhow!("that transfer request was already answered"));
    }
    offer.state = if accept {
        OfferState::Accepted
    } else {
        OfferState::Declined
    };
    Ok((offer.peer_id.clone(), offer.transfer_id))
}
