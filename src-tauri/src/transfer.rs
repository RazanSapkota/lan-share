//! The transfer table and the inbound file-write path.
//!
//! Everything a peer sends is attacker-controlled until proven otherwise, so
//! the write path is deliberately paranoid: the filename is validated as a
//! single path segment, resolved inside the receive folder, written to a
//! random `.part` file, and renamed into place only after a byte-exact
//! completion. A truncated or oversized transfer leaves nothing behind.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{anyhow, Result};
use axum::{
    body::Body,
    extract::{ConnectInfo, Path as AxPath, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_core::Stream;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::{
    models::{AppState, OfferState, ServerCtx, Transfer, PART_SUFFIX, TRANSFER_CAP},
    peers, shares,
    utils::{format_bytes, now_ms},
};

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// Start a transfer row and return its id.
pub(crate) fn begin(
    ctx: &ServerCtx,
    direction: &str,
    peer_id: &str,
    peer_name: &str,
    file_count: usize,
    total_bytes: u64,
) -> u64 {
    let id = ctx.next_transfer_id.fetch_add(1, Ordering::SeqCst) + 1;
    let now = now_ms();
    if let Ok(mut table) = ctx.transfers.lock() {
        table.push_front(Transfer {
            id,
            direction: direction.to_string(),
            peer_id: peer_id.to_string(),
            peer_name: peer_name.to_string(),
            file_name: String::new(),
            file_index: 0,
            file_count,
            bytes: 0,
            total_bytes,
            file_bytes: 0,
            file_total_bytes: 0,
            status: "active".to_string(),
            started_at_ms: now,
            updated_at_ms: now,
            error: None,
        });
        while table.len() > TRANSFER_CAP {
            table.pop_back();
        }
    }
    id
}

/// Same, from the desktop side where only `AppState` is in scope.
pub(crate) fn begin_state(
    state: &AppState,
    direction: &str,
    peer_id: &str,
    peer_name: &str,
    file_count: usize,
    total_bytes: u64,
) -> u64 {
    let id = state.next_transfer_id.fetch_add(1, Ordering::SeqCst) + 1;
    let now = now_ms();
    if let Ok(mut table) = state.transfers.lock() {
        table.push_front(Transfer {
            id,
            direction: direction.to_string(),
            peer_id: peer_id.to_string(),
            peer_name: peer_name.to_string(),
            file_name: String::new(),
            file_index: 0,
            file_count,
            bytes: 0,
            total_bytes,
            file_bytes: 0,
            file_total_bytes: 0,
            status: "active".to_string(),
            started_at_ms: now,
            updated_at_ms: now,
            error: None,
        });
        while table.len() > TRANSFER_CAP {
            table.pop_back();
        }
    }
    id
}

fn with_row<F: FnOnce(&mut Transfer)>(
    table: &Arc<std::sync::Mutex<std::collections::VecDeque<Transfer>>>,
    id: u64,
    edit: F,
) {
    if let Ok(mut rows) = table.lock() {
        if let Some(row) = rows.iter_mut().find(|t| t.id == id) {
            edit(row);
            row.updated_at_ms = now_ms();
        }
    }
}

pub(crate) fn start_file(ctx: &ServerCtx, id: u64, index: usize, name: &str, size: u64) {
    with_row(&ctx.transfers, id, |row| {
        row.file_index = index;
        row.file_name = name.to_string();
        row.file_bytes = 0;
        row.file_total_bytes = size;
        row.status = "active".to_string();
    });
}

pub(crate) fn advance(ctx: &ServerCtx, id: u64, delta: u64) {
    with_row(&ctx.transfers, id, |row| {
        row.bytes += delta;
        row.file_bytes += delta;
    });
}

pub(crate) fn finish(state: &AppState, id: u64, status: &str, error: Option<String>) {
    with_row(&state.transfers, id, |row| {
        row.status = status.to_string();
        row.error = error.clone();
    });
    if let Ok(mut cancels) = state.transfer_cancels.lock() {
        cancels.remove(&id);
    }
}

pub(crate) fn finish_ctx(ctx: &ServerCtx, id: u64, status: &str, error: Option<String>) {
    with_row(&ctx.transfers, id, |row| {
        row.status = status.to_string();
        row.error = error.clone();
    });
    if let Ok(mut cancels) = ctx.transfer_cancels.lock() {
        cancels.remove(&id);
    }
}

pub(crate) fn snapshot(state: &AppState) -> Vec<Transfer> {
    state
        .transfers
        .lock()
        .map(|t| t.iter().cloned().collect())
        .unwrap_or_default()
}

pub(crate) fn cancel_flag(state: &AppState, id: u64) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    if let Ok(mut cancels) = state.transfer_cancels.lock() {
        cancels.insert(id, Arc::clone(&flag));
    }
    flag
}

pub(crate) fn cancel_flag_ctx(ctx: &ServerCtx, id: u64) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    if let Ok(mut cancels) = ctx.transfer_cancels.lock() {
        cancels.insert(id, Arc::clone(&flag));
    }
    flag
}

pub(crate) fn request_cancel(state: &AppState, id: u64) -> bool {
    let flagged = state
        .transfer_cancels
        .lock()
        .ok()
        .and_then(|c| c.get(&id).map(|f| f.store(true, Ordering::SeqCst)))
        .is_some();
    with_row(&state.transfers, id, |row| {
        if !row.is_terminal() {
            row.status = "cancelled".to_string();
        }
    });
    flagged
}

/// Drop finished rows once they have been on screen long enough to be seen.
pub(crate) fn sweep_transfers(ctx: &ServerCtx) {
    const KEEP_TERMINAL_MS: u64 = 120_000;
    if let Ok(mut table) = ctx.transfers.lock() {
        let now = now_ms();
        table.retain(|t| {
            !t.is_terminal() || now.saturating_sub(t.updated_at_ms) <= KEEP_TERMINAL_MS
        });
    }
}

// ---------------------------------------------------------------------------
// PUT /api/peer/file/{offer_id}/{index}
// ---------------------------------------------------------------------------

/// Receive one file of an accepted offer.
///
/// Raw body, not multipart: we own both ends of this protocol, so making the
/// receiver parse RFC 7578 boundaries would be work with no payoff.
pub(crate) async fn receive_file(
    State(ctx): State<ServerCtx>,
    ConnectInfo(peer_addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    AxPath((offer_id, index)): AxPath<(String, usize)>,
    body: Body,
) -> Response {
    let Some(peer) = peers::peer_from_headers(&ctx, &headers) else {
        return not_found();
    };

    // Snapshot what we need, then drop the lock: the write below is long.
    let (expected_name, expected_size, transfer_id) = {
        let Ok(offers) = ctx.offers.lock() else {
            return not_found();
        };
        let Some(offer) = offers.get(&offer_id) else {
            return not_found();
        };
        // A peer can only push into its own offer.
        if offer.peer_id != peer.device_id {
            return not_found();
        }
        match offer.state {
            OfferState::Accepted => {}
            // Not yet answered, or refused. Either way no bytes may land.
            OfferState::Pending | OfferState::Declined => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "error": "not_accepted" })),
                )
                    .into_response()
            }
        }
        if offer.received.contains(&index) {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "already_sent" })),
            )
                .into_response();
        }
        let Some(file) = offer.files.get(index) else {
            return not_found();
        };
        (file.name.clone(), file.size, offer.transfer_id)
    };

    // Content-Length is required: without it there is no way to tell a
    // completed transfer from one the sender abandoned midway, and we would
    // rename a truncated file into place as though it were whole.
    let declared: u64 = match headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
    {
        Some(n) => n,
        None => {
            return (
                StatusCode::LENGTH_REQUIRED,
                Json(json!({ "error": "length_required" })),
            )
                .into_response()
        }
    };
    if declared != expected_size {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "size_mismatch" })),
        )
            .into_response();
    }

    // The name comes from the OFFER the user approved, never from a header on
    // this request -- otherwise the prompt and the file on disk could differ.
    let dir = ctx.receive_dir.clone();
    let final_path = match shares::resolve_new_within(&dir, "", &expected_name) {
        Ok(path) => path,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "bad_filename" })),
            )
                .into_response()
        }
    };
    let Some(parent) = final_path.parent().map(|p| p.to_path_buf()) else {
        return not_found();
    };

    start_file(&ctx, transfer_id, index, &expected_name, expected_size);
    let cancel = cancel_flag_ctx(&ctx, transfer_id);

    // A random temp name, so two peers sending `photo.jpg` at the same moment
    // cannot land on the same partial file.
    let part_path = parent.join(format!("{}{}", crate::auth::random_token(), PART_SUFFIX));

    let written = stream_to_file(&ctx, transfer_id, &part_path, body, declared, &cancel).await;

    let bytes = match written {
        Ok(n) => n,
        Err(err) => {
            let _ = tokio::fs::remove_file(&part_path).await;
            let cancelled = cancel.load(Ordering::SeqCst);
            finish_ctx(
                &ctx,
                transfer_id,
                if cancelled { "cancelled" } else { "failed" },
                Some(err.to_string()),
            );
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "write_failed" })),
            )
                .into_response();
        }
    };

    // Byte-exact or nothing. A short body is a failed transfer, not a small
    // file, and renaming it into place would present corruption as success.
    if bytes != declared {
        let _ = tokio::fs::remove_file(&part_path).await;
        finish_ctx(&ctx, transfer_id, "failed", Some("incomplete".to_string()));
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "incomplete" })),
        )
            .into_response();
    }

    // Only now pick the visible name, so a file that appeared in the folder
    // while we were writing still cannot be clobbered.
    let Some(target) = crate::utils::unique_destination(&parent, &expected_name) else {
        let _ = tokio::fs::remove_file(&part_path).await;
        finish_ctx(&ctx, transfer_id, "failed", Some("name taken".to_string()));
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "name_taken" })),
        )
            .into_response();
    };
    if tokio::fs::rename(&part_path, &target).await.is_err() {
        let _ = tokio::fs::remove_file(&part_path).await;
        finish_ctx(&ctx, transfer_id, "failed", Some("rename failed".to_string()));
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "write_failed" })),
        )
            .into_response();
    }

    let saved_as = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| expected_name.clone());

    let complete = {
        let mut done = false;
        if let Ok(mut offers) = ctx.offers.lock() {
            if let Some(offer) = offers.get_mut(&offer_id) {
                offer.received.push(index);
                done = offer.received.len() == offer.files.len();
            }
        }
        done
    };
    if complete {
        finish_ctx(&ctx, transfer_id, "done", None);
        if let Ok(mut offers) = ctx.offers.lock() {
            offers.remove(&offer_id);
        }
    }

    ctx.log_event(
        "receive",
        "ok",
        &peer_addr.ip().to_string(),
        &format!("peer:{}", peer.name),
        None,
        Some(saved_as.clone()),
        Some(format_bytes(bytes)),
    );

    Json(json!({ "ok": true, "savedAs": saved_as, "bytes": bytes })).into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response()
}

/// Stream a request body to disk, stopping at `limit` bytes.
async fn stream_to_file(
    ctx: &ServerCtx,
    transfer_id: u64,
    path: &PathBuf,
    body: Body,
    limit: u64,
    cancel: &Arc<AtomicBool>,
) -> Result<u64> {
    use futures_util::StreamExt;

    let mut file = tokio::fs::File::create(path).await?;
    let mut stream = body.into_data_stream();
    let mut written: u64 = 0;

    while let Some(chunk) = std::pin::Pin::new(&mut stream).next().await {
        if cancel.load(Ordering::SeqCst) {
            return Err(anyhow!("cancelled"));
        }
        let chunk = chunk.map_err(|e| anyhow!("read failed: {e}"))?;
        // A sender that ignores its own Content-Length must not be able to
        // fill the disk. Stop one byte past the declaration so the mismatch is
        // detectable rather than silently truncated to exactly `limit`.
        if written + chunk.len() as u64 > limit {
            let take = (limit.saturating_sub(written) + 1).min(chunk.len() as u64) as usize;
            file.write_all(&chunk[..take]).await?;
            written += take as u64;
            break;
        }
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        advance(ctx, transfer_id, chunk.len() as u64);
    }

    file.flush().await?;
    // Explicit, so the rename below cannot beat the data to disk.
    file.sync_all().await.ok();
    drop(file);
    Ok(written)
}

/// Delete abandoned `.part` files. Called on startup: a crash mid-transfer is
/// the one case nothing else cleans up.
pub(crate) fn sweep_parts(dir: &std::path::Path, older_than_ms: u64) -> u64 {
    let mut removed = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .map(|n| n.to_string_lossy().ends_with(PART_SUFFIX))
            .unwrap_or(false)
        {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .map(|d| d.as_millis() as u64 >= older_than_ms)
            .unwrap_or(true);
        if old && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Keeps `futures_core::Stream` referenced so the import is not dead when the
/// body-stream type changes shape between axum versions.
#[allow(dead_code)]
fn _assert_stream<S: Stream>(_: &S) {}
