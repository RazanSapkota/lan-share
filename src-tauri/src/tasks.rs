//! Every `#[tauri::command]`, plus the background-task helpers. This is the
//! only module `main.rs` imports commands from, mirroring the reference app.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
};

use anyhow::Result;
use tauri::State;

use crate::{
    activity, auth, config, media,
    models::{
        ActivityEntry, AppConfig, AppState, DiscoveredPeer, DiscoveredPeerView,
        DiscoveryStatus, DownloadResult, FirewallHint, IndexShareResponse, LanUrl,
        PairPromptView, PairResult, Peer, PeerView, PrewarmResponse, ProgressPayload, QrPayload,
        SaveConfigResult, ServerSettings, ServerStatus, Session, SessionView, Share, ShareView,
        TaskHandle, ThumbCacheStats,
    },
    net, server, shares, utils,
};

// ---------------------------------------------------------------------------
// Task helpers -- structurally identical to the reference app
// ---------------------------------------------------------------------------

pub(crate) fn new_progress_payload(phase: &str, message: &str) -> ProgressPayload {
    ProgressPayload {
        phase: phase.to_string(),
        message: message.to_string(),
        ..Default::default()
    }
}

pub(crate) fn set_task_progress(
    tasks: &Arc<Mutex<HashMap<u64, ProgressPayload>>>,
    task_id: u64,
    phase: &str,
    progress: f64,
    message: impl Into<String>,
) {
    if let Ok(mut guard) = tasks.lock() {
        if let Some(payload) = guard.get_mut(&task_id) {
            payload.phase = phase.to_string();
            payload.progress = progress.clamp(0.0, 1.0);
            payload.message = message.into();
        }
    }
}

fn finish_index_task(
    tasks: &Arc<Mutex<HashMap<u64, ProgressPayload>>>,
    task_id: u64,
    result: Result<IndexShareResponse>,
) {
    if let Ok(mut guard) = tasks.lock() {
        if let Some(payload) = guard.get_mut(&task_id) {
            payload.done = true;
            payload.progress = 1.0;
            match result {
                Ok(response) => {
                    payload.phase = "done".to_string();
                    payload.message = format!(
                        "{} files, {}",
                        response.file_count, response.total_bytes_text
                    );
                    payload.index_result = Some(response);
                }
                Err(err) => {
                    payload.phase = "error".to_string();
                    payload.error = Some(err.to_string());
                }
            }
        }
    }
}

fn finish_prewarm_task(
    tasks: &Arc<Mutex<HashMap<u64, ProgressPayload>>>,
    task_id: u64,
    result: Result<PrewarmResponse>,
) {
    if let Ok(mut guard) = tasks.lock() {
        if let Some(payload) = guard.get_mut(&task_id) {
            payload.done = true;
            payload.progress = 1.0;
            match result {
                Ok(response) => {
                    payload.phase = "done".to_string();
                    payload.message = format!(
                        "{} generated, {} cached, {} failed",
                        response.generated, response.reused, response.failed
                    );
                    payload.prewarm_result = Some(response);
                }
                Err(err) => {
                    payload.phase = "error".to_string();
                    payload.error = Some(err.to_string());
                }
            }
        }
    }
}

fn alloc_task(state: &AppState, phase: &str, message: &str) -> Result<u64, String> {
    let task_id = state.next_task_id.fetch_add(1, Ordering::SeqCst) + 1;
    let mut guard = state
        .tasks
        .lock()
        .map_err(|_| "task state poisoned".to_string())?;
    guard.insert(task_id, new_progress_payload(phase, message));
    Ok(task_id)
}

#[tauri::command]
pub(crate) fn get_task_progress(
    task_id: u64,
    state: State<'_, AppState>,
) -> Result<ProgressPayload, String> {
    let guard = state
        .tasks
        .lock()
        .map_err(|_| "task state poisoned".to_string())?;
    guard
        .get(&task_id)
        .cloned()
        .ok_or_else(|| format!("task {task_id} not found"))
}

#[tauri::command]
pub(crate) fn clear_task(task_id: u64, state: State<'_, AppState>) -> Result<(), String> {
    let mut guard = state
        .tasks
        .lock()
        .map_err(|_| "task state poisoned".to_string())?;
    guard.remove(&task_id);
    drop(guard);
    if let Ok(mut cancels) = state.cancellations.lock() {
        cancels.remove(&task_id);
    }
    Ok(())
}

/// Flip the cancel flag for `task_id` if one was registered. Tasks that opt in
/// poll the flag at safe points and bail out gracefully.
#[tauri::command]
pub(crate) fn cancel_task(task_id: u64, state: State<'_, AppState>) -> Result<bool, String> {
    let cancels = state
        .cancellations
        .lock()
        .map_err(|_| "cancellation state poisoned".to_string())?;
    if let Some(flag) = cancels.get(&task_id) {
        flag.store(true, Ordering::SeqCst);
        Ok(true)
    } else {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[tauri::command]
pub(crate) fn load_config(app: tauri::AppHandle) -> AppConfig {
    config::load_config_impl(&app)
}

#[tauri::command]
pub(crate) fn save_config(
    app: tauri::AppHandle,
    config: AppConfig,
    state: State<'_, AppState>,
) -> Result<SaveConfigResult, String> {
    let previous = state
        .settings
        .read()
        .map(|s| s.clone())
        .unwrap_or_default();

    let mut next = config;
    config::normalize(&mut next);

    config::save_config_impl(&app, &next).map_err(|e| e.to_string())?;
    config::apply_config_to_state(&state, &next).map_err(|e| e.to_string())?;

    let incoming = ServerSettings::from_config(&next);
    let mut result = SaveConfigResult {
        port: next.port,
        ..Default::default()
    };

    // Only bind parameters need the listener torn down; everything else is read
    // live off the RwLock by the handlers. Auto-restarting is the right call --
    // a user who changed the port expects the port to change, and an "click
    // Restart to apply" banner is just one more thing to forget.
    if previous.needs_rebind(&incoming) && server::is_running(&state) {
        match server::restart_server_impl(&app, &state) {
            Ok(status) => {
                result.rebound = true;
                result.port = status.port;
                if status.port != next.port {
                    result.warning = Some(format!(
                        "Port {} was taken; now listening on {}.",
                        next.port, status.port
                    ));
                }
            }
            Err(err) => {
                result.warning = Some(format!("Server could not restart: {err}"));
            }
        }
    }

    Ok(result)
}

/// Load, mutate, save, and re-apply in one step, so every share/PIN command
/// keeps the file and the live state in sync without repeating itself.
fn mutate_config<F, T>(
    app: &tauri::AppHandle,
    state: &AppState,
    mutate: F,
) -> Result<T, String>
where
    F: FnOnce(&mut AppConfig) -> Result<T, String>,
{
    let mut config = config::load_config_impl(app);
    let out = mutate(&mut config)?;
    config::normalize(&mut config);
    config::save_config_impl(app, &config).map_err(|e| e.to_string())?;
    config::apply_config_to_state(state, &config).map_err(|e| e.to_string())?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

#[tauri::command]
pub(crate) fn start_server(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<ServerStatus, String> {
    // Generate a PIN on first start rather than at install time, so the value
    // the Dashboard shows is always the value the server is checking.
    let needs_pin = state
        .settings
        .read()
        .map(|s| s.pin_enabled && s.pin.trim().is_empty())
        .unwrap_or(false);
    if needs_pin {
        mutate_config(&app, &state, |config| {
            config.pin = auth::random_pin();
            Ok(())
        })?;
    }

    server::start_server_impl(&app, &state).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn stop_server(state: State<'_, AppState>) -> Result<ServerStatus, String> {
    server::stop_server_impl(&state).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn restart_server(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<ServerStatus, String> {
    server::restart_server_impl(&app, &state).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn get_server_status(state: State<'_, AppState>) -> ServerStatus {
    server::status_impl(&state)
}

// ---------------------------------------------------------------------------
// Shares
// ---------------------------------------------------------------------------

/// The address share links are built from: the primary interface, else the
/// first non-virtual one. Same ranking the Dashboard QR uses, so the two never
/// disagree about which address to hand out.
fn link_host() -> Option<String> {
    let addresses = net::lan_addresses();
    addresses
        .iter()
        .find(|a| a.is_primary)
        .or_else(|| addresses.iter().find(|a| !a.is_virtual))
        .map(|a| a.ip.clone())
}

fn share_view(
    share: &crate::models::ResolvedShare,
    port: u16,
    host: Option<&str>,
) -> ShareView {
    let link = host.map(|host| format!("http://{}:{}/s/{}", host, port, share.cfg.token));
    ShareView {
        root_exists: share.root_exists,
        display_path: shares::display_path(&share.root),
        link,
        cfg: share.cfg.clone(),
    }
}

#[tauri::command]
pub(crate) fn list_shares(state: State<'_, AppState>) -> Result<Vec<ShareView>, String> {
    let status = server::status_impl(&state);
    // Enumerated once per call, not once per share: each lookup walks every
    // network adapter on the machine.
    let host = if status.running { link_host() } else { None };
    let registry = state
        .shares
        .read()
        .map_err(|_| "share state poisoned".to_string())?;
    Ok(registry
        .shares
        .iter()
        .map(|s| share_view(s, status.port, host.as_deref()))
        .collect())
}

#[tauri::command]
pub(crate) fn add_share(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    name: Option<String>,
) -> Result<ShareView, String> {
    let added = add_shares(app, state, vec![path])?;
    let mut view = added
        .into_iter()
        .next()
        .ok_or_else(|| "failed to add share".to_string())?;
    if let Some(name) = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) {
        view.cfg.name = name;
    }
    Ok(view)
}

#[tauri::command]
pub(crate) fn add_shares(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<Vec<ShareView>, String> {
    let ids = mutate_config(&app, &state, |config| {
        let mut ids = Vec::new();
        for raw in paths {
            let path = raw.trim().to_string();
            if path.is_empty() {
                continue;
            }
            // Canonicalize before the duplicate check, so "D:\Media" and
            // "D:\media\..\Media" are recognised as the same folder.
            let canonical = shares::canonical_root(&path).map_err(|e| e.to_string())?;
            let is_file = canonical.is_file();

            let already = config.shares.iter().any(|existing| {
                shares::canonical_root(&existing.path)
                    .map(|r| r == canonical)
                    .unwrap_or(false)
            });
            if already {
                return Err(format!("{} is already shared", shares::display_path(&canonical)));
            }

            let name = canonical
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());

            let id = auth::random_id();
            ids.push(id.clone());
            config.shares.push(Share {
                id,
                name,
                // Store the ORIGINAL path, not the verbatim canonical form:
                // the config is user-readable, and "\\?\D:\Media" is not.
                path: shares::display_path(&canonical),
                token: auth::random_token(),
                enabled: true,
                is_inbox: false,
                read_only: true,
                is_file,
                recursive: true,
                include_names: Vec::new(),
                include_ext: Vec::new(),
                exclude_ext: Vec::new(),
                added_ms: utils::now_ms(),
                note: None,
            });
        }
        Ok(ids)
    })?;

    let status = server::status_impl(&state);
    // Enumerated once per call, not once per share: each lookup walks every
    // network adapter on the machine.
    let host = if status.running { link_host() } else { None };
    let registry = state
        .shares
        .read()
        .map_err(|_| "share state poisoned".to_string())?;
    Ok(ids
        .iter()
        .filter_map(|id| registry.by_id(id))
        .map(|s| share_view(s, status.port, host.as_deref()))
        .collect())
}

#[tauri::command]
pub(crate) fn remove_share(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    share_id: String,
) -> Result<(), String> {
    mutate_config(&app, &state, |config| {
        let before = config.shares.len();
        config.shares.retain(|s| s.id != share_id);
        if config.shares.len() == before {
            return Err(format!("share {share_id} not found"));
        }
        if config.inbox_share_id.as_deref() == Some(share_id.as_str()) {
            config.inbox_share_id = None;
        }
        Ok(())
    })
}

#[tauri::command]
pub(crate) fn update_share(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    share: Share,
) -> Result<ShareView, String> {
    let id = share.id.clone();
    mutate_config(&app, &state, |config| {
        let existing = config
            .shares
            .iter_mut()
            .find(|s| s.id == share.id)
            .ok_or_else(|| format!("share {} not found", share.id))?;
        // The id and token are not editable through this path: the id appears
        // in handed-out URLs, and the token has its own regenerate command so
        // rotating a secret is always a deliberate act.
        let token = existing.token.clone();
        *existing = Share { token, ..share };
        Ok(())
    })?;

    let status = server::status_impl(&state);
    // Enumerated once per call, not once per share: each lookup walks every
    // network adapter on the machine.
    let host = if status.running { link_host() } else { None };
    let registry = state
        .shares
        .read()
        .map_err(|_| "share state poisoned".to_string())?;
    registry
        .by_id(&id)
        .map(|s| share_view(s, status.port, host.as_deref()))
        .ok_or_else(|| format!("share {id} not found"))
}

#[tauri::command]
pub(crate) fn set_share_enabled(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    share_id: String,
    enabled: bool,
) -> Result<(), String> {
    mutate_config(&app, &state, |config| {
        let share = config
            .shares
            .iter_mut()
            .find(|s| s.id == share_id)
            .ok_or_else(|| format!("share {share_id} not found"))?;
        share.enabled = enabled;
        Ok(())
    })
}

#[tauri::command]
pub(crate) fn regenerate_share_token(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    share_id: String,
) -> Result<String, String> {
    // `apply_config_to_state` prunes sessions whose stored token no longer
    // matches, so the old link stops working immediately -- for anyone already
    // browsing through it, not just for new visitors.
    mutate_config(&app, &state, |config| {
        let share = config
            .shares
            .iter_mut()
            .find(|s| s.id == share_id)
            .ok_or_else(|| format!("share {share_id} not found"))?;
        share.token = auth::random_token();
        Ok(share.token.clone())
    })
}

#[tauri::command]
pub(crate) fn set_inbox_share(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    share_id: Option<String>,
) -> Result<(), String> {
    mutate_config(&app, &state, |config| {
        if let Some(id) = &share_id {
            let share = config
                .shares
                .iter_mut()
                .find(|s| &s.id == id)
                .ok_or_else(|| format!("share {id} not found"))?;
            if share.is_file {
                return Err("a single-file share cannot be an inbox".to_string());
            }
        }
        config.inbox_share_id = share_id;
        // `normalize` applies the at-most-one-inbox rule and clears read_only
        // on the winner.
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// PIN
// ---------------------------------------------------------------------------

#[tauri::command]
pub(crate) fn get_pin(app: tauri::AppHandle) -> String {
    config::load_config_impl(&app).pin
}

#[tauri::command]
pub(crate) fn set_pin(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    pin: String,
) -> Result<(), String> {
    let trimmed = pin.trim().to_string();
    if !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Err("PIN must contain digits only".to_string());
    }
    if trimmed.len() < 4 || trimmed.len() > 12 {
        return Err("PIN must be 4 to 12 digits".to_string());
    }
    mutate_config(&app, &state, |config| {
        config.pin = trimmed;
        Ok(())
    })
}

#[tauri::command]
pub(crate) fn generate_pin(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    mutate_config(&app, &state, |config| {
        config.pin = auth::random_pin();
        Ok(config.pin.clone())
    })
}

#[tauri::command]
pub(crate) fn set_pin_enabled(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    mutate_config(&app, &state, |config| {
        config.pin_enabled = enabled;
        if enabled && config.pin.trim().is_empty() {
            config.pin = auth::random_pin();
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

#[tauri::command]
pub(crate) fn get_lan_urls(state: State<'_, AppState>) -> Vec<LanUrl> {
    let status = server::status_impl(&state);
    net::lan_urls(status.port, None)
}

#[tauri::command]
pub(crate) fn get_share_qr(
    state: State<'_, AppState>,
    share_id: Option<String>,
    ip: Option<String>,
    size: u32,
) -> Result<QrPayload, String> {
    let status = server::status_impl(&state);

    let host = match ip.filter(|s| !s.trim().is_empty()) {
        Some(ip) => ip,
        None => net::lan_addresses()
            .into_iter()
            .next()
            .map(|a| a.ip)
            // Offline (no adapter at all) still needs a QR that renders rather
            // than an error the Dashboard has to special-case.
            .unwrap_or_else(|| "127.0.0.1".to_string()),
    };

    let path = match share_id {
        Some(id) => {
            let registry = state
                .shares
                .read()
                .map_err(|_| "share state poisoned".to_string())?;
            let share = registry
                .by_id(&id)
                .ok_or_else(|| format!("share {id} not found"))?;
            format!("/s/{}", share.cfg.token)
        }
        None => String::new(),
    };

    let url = format!("http://{}:{}{}", host, status.port, path);
    let svg = net::qr_svg(&url, size.clamp(96, 1024)).map_err(|e| e.to_string())?;
    Ok(QrPayload { url, svg })
}

#[tauri::command]
pub(crate) fn get_qr_for_url(url: String, size: u32) -> Result<String, String> {
    net::qr_svg(&url, size.clamp(96, 1024)).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn get_firewall_hint(state: State<'_, AppState>) -> FirewallHint {
    let status = server::status_impl(&state);
    FirewallHint {
        platform: std::env::consts::OS.to_string(),
        command: net::firewall_command(status.port),
        note: net::firewall_note(),
    }
}

// ---------------------------------------------------------------------------
// Activity / sessions
// ---------------------------------------------------------------------------

#[tauri::command]
pub(crate) fn get_activity_log(
    state: State<'_, AppState>,
    since_id: u64,
    limit: usize,
) -> Result<Vec<ActivityEntry>, String> {
    let guard = state
        .activity
        .lock()
        .map_err(|_| "activity state poisoned".to_string())?;
    Ok(activity::read_since(&guard, since_id, limit))
}

#[tauri::command]
pub(crate) fn clear_activity_log(state: State<'_, AppState>) -> Result<(), String> {
    let mut guard = state
        .activity
        .lock()
        .map_err(|_| "activity state poisoned".to_string())?;
    guard.clear();
    Ok(())
}

#[tauri::command]
pub(crate) fn list_sessions(state: State<'_, AppState>) -> Result<Vec<SessionView>, String> {
    let guard = state
        .sessions
        .lock()
        .map_err(|_| "session state poisoned".to_string())?;
    let mut out: Vec<SessionView> = guard
        .iter()
        .map(|(token, session)| session_view(token, session))
        .collect();
    out.sort_by(|a, b| b.last_seen_ms.cmp(&a.last_seen_ms));
    Ok(out)
}

fn session_view(token: &str, session: &Session) -> SessionView {
    SessionView {
        // Truncated: the desktop needs to identify a session to revoke it, not
        // to be able to replay it if this list ever leaks into a log.
        token: token.chars().take(8).collect(),
        scope: session.scope_name().to_string(),
        share_id: session.share_id().map(|s| s.to_string()),
        client_ip: session.client_ip.clone(),
        user_agent: session.user_agent.clone(),
        created_ms: session.created_ms,
        last_seen_ms: session.last_seen_ms,
    }
}

#[tauri::command]
pub(crate) fn revoke_session(
    state: State<'_, AppState>,
    token: String,
) -> Result<bool, String> {
    let mut guard = state
        .sessions
        .lock()
        .map_err(|_| "session state poisoned".to_string())?;
    // The UI only holds the truncated prefix, so match on that.
    let full = guard
        .keys()
        .find(|k| k.starts_with(&token))
        .cloned();
    Ok(match full {
        Some(key) => guard.remove(&key).is_some(),
        None => false,
    })
}

#[tauri::command]
pub(crate) fn revoke_all_sessions(state: State<'_, AppState>) -> Result<usize, String> {
    let mut guard = state
        .sessions
        .lock()
        .map_err(|_| "session state poisoned".to_string())?;
    let count = guard.len();
    guard.clear();
    Ok(count)
}

// ---------------------------------------------------------------------------
// Thumbnails
// ---------------------------------------------------------------------------

fn resolved_thumb_dir(app: &tauri::AppHandle, state: &AppState) -> Result<PathBuf, String> {
    if let Ok(guard) = state.thumb_dir.lock() {
        if let Some(dir) = guard.as_ref() {
            return Ok(dir.clone());
        }
    }
    media::thumb_dir(app).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn get_thumb_cache_stats(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<ThumbCacheStats, String> {
    let dir = resolved_thumb_dir(&app, &state)?;
    let (file_count, total_bytes) = media::cache_stats(&dir);
    Ok(ThumbCacheStats {
        file_count,
        total_bytes,
        total_bytes_text: utils::format_bytes(total_bytes),
        path: shares::display_path(&dir),
    })
}

#[tauri::command]
pub(crate) fn clear_thumb_cache(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<u64, String> {
    let dir = resolved_thumb_dir(&app, &state)?;
    media::clear_cache(&dir).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn start_prewarm_thumbs_task(
    app: tauri::AppHandle,
    share_id: String,
    state: State<'_, AppState>,
) -> Result<TaskHandle, String> {
    let task_id = alloc_task(&state, "prewarm_starting", "Preparing thumbnails")?;

    let dir = resolved_thumb_dir(&app, &state)?;
    let (edge, quality, cache_cap) = {
        let s = state
            .settings
            .read()
            .map_err(|_| "settings poisoned".to_string())?;
        (s.thumb_max_edge, s.thumb_quality, 0u64)
    };
    let cache_cap = if cache_cap == 0 {
        config::load_config_impl(&app).thumb_cache_max_mb * 1024 * 1024
    } else {
        cache_cap
    };

    let share = {
        let registry = state
            .shares
            .read()
            .map_err(|_| "share state poisoned".to_string())?;
        registry
            .by_id(&share_id)
            .cloned()
            .ok_or_else(|| format!("share {share_id} not found"))?
    };

    let tasks = Arc::clone(&state.tasks);
    let cancel = Arc::new(AtomicBool::new(false));
    if let Ok(mut cancels) = state.cancellations.lock() {
        cancels.insert(task_id, Arc::clone(&cancel));
    }

    thread::spawn(move || {
        let result = (|| -> Result<PrewarmResponse> {
            set_task_progress(&tasks, task_id, "prewarm_scanning", 0.0, "Scanning for images");

            let mut candidates: Vec<PathBuf> = Vec::new();
            let mut walker = walkdir::WalkDir::new(&share.root).follow_links(false);
            if !share.cfg.recursive {
                walker = walker.max_depth(1);
            }
            for entry in walker.into_iter().flatten() {
                if cancel.load(Ordering::SeqCst) {
                    break;
                }
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if media::can_thumbnail(&utils::ext_of(&path.to_string_lossy())) {
                    candidates.push(path.to_path_buf());
                }
            }

            let total = candidates.len().max(1);
            let mut response = PrewarmResponse {
                share_id: share.cfg.id.clone(),
                ..Default::default()
            };

            for (index, path) in candidates.iter().enumerate() {
                if cancel.load(Ordering::SeqCst) {
                    break;
                }
                // A cache hit is a plain file read, so "reused" is measured by
                // whether the cache entry already existed before we asked.
                let existed = std::fs::metadata(path)
                    .ok()
                    .and_then(|m| {
                        let mtime = m.modified().ok().map(utils::system_time_ms)?;
                        Some(media::cache_path(
                            &dir,
                            &media::cache_key(path, mtime, m.len(), edge, quality),
                        ))
                    })
                    .map(|p| p.exists())
                    .unwrap_or(false);

                match media::thumbnail(path, &dir, edge, quality) {
                    Ok(_) if existed => response.reused += 1,
                    Ok(_) => response.generated += 1,
                    Err(_) => response.failed += 1,
                }

                if index % 8 == 0 {
                    set_task_progress(
                        &tasks,
                        task_id,
                        "prewarm_running",
                        index as f64 / total as f64,
                        format!("{} / {} images", index + 1, candidates.len()),
                    );
                }
            }

            // Trimming after the run rather than per request: walking the cache
            // costs a stat per file, which is not a price to pay on a fetch.
            let _ = media::evict_to(&dir, cache_cap);
            Ok(response)
        })();

        finish_prewarm_task(&tasks, task_id, result);
    });

    Ok(TaskHandle { id: task_id })
}

// ---------------------------------------------------------------------------
// Indexing
// ---------------------------------------------------------------------------

#[tauri::command]
pub(crate) fn start_index_share_task(
    share_id: String,
    state: State<'_, AppState>,
) -> Result<TaskHandle, String> {
    let task_id = alloc_task(&state, "index_starting", "Counting files")?;

    let share = {
        let registry = state
            .shares
            .read()
            .map_err(|_| "share state poisoned".to_string())?;
        registry
            .by_id(&share_id)
            .cloned()
            .ok_or_else(|| format!("share {share_id} not found"))?
    };

    let tasks = Arc::clone(&state.tasks);

    thread::spawn(move || {
        let result = (|| -> Result<IndexShareResponse> {
            let (files, dirs, bytes, skipped) = shares::index_share(&share, |files, bytes| {
                set_task_progress(
                    &tasks,
                    task_id,
                    "index_running",
                    // A directory walk has no knowable total, so the bar stays
                    // indeterminate and the message carries the real signal.
                    0.0,
                    format!("{} files, {}", files, utils::format_bytes(bytes)),
                );
            });
            Ok(IndexShareResponse {
                share_id: share.cfg.id.clone(),
                file_count: files,
                dir_count: dirs,
                total_bytes: bytes,
                total_bytes_text: utils::format_bytes(bytes),
                skipped,
            })
        })();

        finish_index_task(&tasks, task_id, result);
    });

    Ok(TaskHandle { id: task_id })
}

// ---------------------------------------------------------------------------
// Dialogs / OS
// ---------------------------------------------------------------------------

#[tauri::command]
pub(crate) fn pick_folder() -> Option<String> {
    rfd::FileDialog::new()
        .pick_folder()
        .map(|path| path.display().to_string())
}

#[tauri::command]
pub(crate) fn pick_files() -> Option<Vec<String>> {
    rfd::FileDialog::new()
        .pick_files()
        .map(|paths| paths.into_iter().map(|p| p.display().to_string()).collect())
}

#[tauri::command]
pub(crate) fn path_exists(path: String) -> bool {
    !path.trim().is_empty() && std::path::Path::new(path.trim()).exists()
}

#[tauri::command]
pub(crate) fn show_in_explorer(path: String) -> Result<(), String> {
    let raw = path.trim();
    if raw.is_empty() {
        return Err("path is empty".to_string());
    }
    // Explorer does not understand the verbatim \\?\ prefix.
    let target = dunce::simplified(std::path::Path::new(raw)).to_path_buf();
    if !target.exists() {
        return Err(format!("path does not exist: {}", target.display()));
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        // `.arg()` quotes in a way that makes Explorer silently drop /select,
        // so the argument string is built by hand. And Explorer returns exit
        // code 1 on SUCCESS, so its status is deliberately not checked.
        let mut command = std::process::Command::new("explorer");
        command.raw_arg(format!("/select,\"{}\"", target.display()));
        command
            .spawn()
            .map_err(|e| format!("failed to open Explorer: {e}"))?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&target)
            .spawn()
            .map_err(|e| format!("failed to open Finder: {e}"))?;
        return Ok(());
    }

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        let dir = if target.is_dir() {
            target.clone()
        } else {
            target.parent().map(|p| p.to_path_buf()).unwrap_or(target)
        };
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map_err(|e| format!("failed to open file manager: {e}"))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// External players
// ---------------------------------------------------------------------------

/// Extensions this command will not hand to the shell.
///
/// `open_in_player` exists to open media with whatever the OS has registered,
/// which means it ends in "run the default handler for this file". For a `.exe`
/// or a `.lnk` the default handler is "execute it", so the list of things a
/// Play button must refuse is exactly the list of things that are programs.
const NEVER_LAUNCH: &[&str] = &[
    "exe", "com", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "msi",
    "msp", "scr", "cpl", "hta", "reg", "lnk", "pif", "url", "inf", "jar", "app", "sh",
];

/// Whether a Play button may hand this file to the shell.
///
/// Pure and separate from the command so the rule is testable without spawning
/// anything -- which is the one thing a test of this must not do.
pub(crate) fn is_launchable(ext: &str) -> bool {
    !NEVER_LAUNCH.contains(&ext.to_ascii_lowercase().as_str())
}

/// Play a local file: the configured player if there is one, otherwise whatever
/// the OS uses for that file type.
#[tauri::command]
pub(crate) fn open_in_player(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let raw = path.trim();
    if raw.is_empty() {
        return Err("path is empty".to_string());
    }
    let target = dunce::simplified(std::path::Path::new(raw)).to_path_buf();
    if !target.is_file() {
        return Err(format!("no such file: {}", target.display()));
    }
    let ext = utils::ext_of(&target.to_string_lossy());
    if !is_launchable(&ext) {
        return Err(format!("{ext} files are programs, not media"));
    }

    let player = config::load_config_impl(&app).external_player;
    launch_player(player.as_deref(), &target.to_string_lossy())
}

/// Hand an argument -- a local path, or a URL for a peer's file -- to the
/// player. Split out because the peer path needs the same launching without the
/// local-file checks above.
pub(crate) fn launch_player(player: Option<&str>, argument: &str) -> Result<(), String> {
    if let Some(player) = player.filter(|p| !p.trim().is_empty()) {
        // `.arg` and not a hand-built command line: this one is a real argv,
        // so a space or a quote in the path is the standard library's problem
        // rather than ours.
        let mut command = std::process::Command::new(player);
        command.arg(argument);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        return command
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("failed to start {player}: {e}"));
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        // `start` needs the empty "" title argument, or it treats a quoted path
        // as the window title and opens a console instead of the file.
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!("/C start \"\" \"{argument}\""));
        command.creation_flags(0x0800_0000);
        command
            .spawn()
            .map_err(|e| format!("failed to open the default player: {e}"))?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(argument)
            .spawn()
            .map_err(|e| format!("failed to open the default player: {e}"))?;
        return Ok(());
    }

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(argument)
            .spawn()
            .map_err(|e| format!("failed to open the default player: {e}"))?;
        Ok(())
    }
}

/// Look for VLC in the places it installs itself, for the Detect button.
///
/// Returns the path rather than setting it, so the user sees what was found
/// before it becomes their setting.
#[tauri::command]
pub(crate) fn detect_player() -> Option<String> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();

    #[cfg(target_os = "windows")]
    {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Ok(base) = std::env::var(var) {
                candidates.push(std::path::PathBuf::from(base).join("VideoLAN\\VLC\\vlc.exe"));
            }
        }
        candidates.push(std::path::PathBuf::from("C:\\Program Files\\VideoLAN\\VLC\\vlc.exe"));
    }
    #[cfg(target_os = "macos")]
    {
        candidates.push(std::path::PathBuf::from("/Applications/VLC.app/Contents/MacOS/VLC"));
        candidates.push(std::path::PathBuf::from("/Applications/IINA.app/Contents/MacOS/IINA"));
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        for dir in ["/usr/bin", "/usr/local/bin", "/snap/bin", "/var/lib/flatpak/exports/bin"] {
            candidates.push(std::path::PathBuf::from(dir).join("vlc"));
            candidates.push(std::path::PathBuf::from(dir).join("mpv"));
        }
    }

    candidates
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
}

#[tauri::command]
pub(crate) fn open_url(url: String) -> Result<(), String> {
    let url = url.trim();
    // Only http(s). Without this, a crafted share name reaching this command
    // could launch `file:///` or a custom protocol handler.
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("only http and https URLs can be opened".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!("/C start \"\" \"{url}\""));
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        command
            .spawn()
            .map_err(|e| format!("failed to open browser: {e}"))?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("failed to open browser: {e}"))?;
        return Ok(());
    }

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("failed to open browser: {e}"))?;
        Ok(())
    }
}

#[tauri::command]
pub(crate) fn format_bytes_command(size: u64) -> String {
    utils::format_bytes(size)
}

// ---------------------------------------------------------------------------
// Peers: discovery
// ---------------------------------------------------------------------------

#[tauri::command]
pub(crate) fn list_discovered(state: State<'_, AppState>) -> DiscoveryStatus {
    let now = utils::now_ms();
    let running = server::is_running(&state);
    let (port, peering) = state
        .settings
        .read()
        .map(|s| (s.discovery_port, s.peering_enabled))
        .unwrap_or((0, false));

    let paired: std::collections::HashSet<String> = state
        .peers
        .read()
        .map(|r| r.peers.iter().map(|p| p.device_id.clone()).collect())
        .unwrap_or_default();

    let mut devices: Vec<DiscoveredPeerView> = state
        .discovered
        .lock()
        .map(|table| {
            table
                .values()
                .map(|peer| DiscoveredPeerView {
                    online: peer.online(now),
                    paired: paired.contains(&peer.device_id),
                    peer: peer.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    // Online first, then most recently heard: the device someone is looking for
    // is almost always the one that just appeared.
    devices.sort_by(|a, b| {
        b.online
            .cmp(&a.online)
            .then(b.peer.last_seen_ms.cmp(&a.peer.last_seen_ms))
    });

    let error = state.discovery_error.lock().ok().and_then(|e| e.clone());
    // The health heuristic listens for our own beacon, so it only says
    // anything while we are actually announcing.
    let health = if !running || !peering {
        "ok".to_string()
    } else if error.is_some() {
        "error".to_string()
    } else {
        discovery_health_of(&state)
    };

    DiscoveryStatus {
        running: running && peering && error.is_none(),
        listening: running && state.discovery_bound.load(Ordering::Relaxed),
        port,
        health,
        error,
        devices,
    }
}

fn discovery_health_of(state: &AppState) -> String {
    crate::discovery::health_from(
        state.discovery_started_ms.load(Ordering::Relaxed),
        state.discovery_self_seen_ms.load(Ordering::Relaxed),
        state.discovered.lock().map(|t| t.is_empty()).unwrap_or(true),
    )
    .to_string()
}

/// Reach a device by address and add it to the discovered list.
///
/// The fallback for networks where broadcast and multicast are both filtered.
/// On a router with client isolation this fails too -- honestly, and with a
/// message, which beats a device list that stays mysteriously empty.
#[tauri::command]
pub(crate) async fn add_peer_by_address(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    address: String,
) -> Result<DiscoveredPeer, String> {
    let default_port = state
        .settings
        .read()
        .map(|s| s.port)
        .unwrap_or(crate::models::DEFAULT_PORT);
    let (ip, port) =
        crate::discovery::parse_manual_address(&address, default_port).map_err(|e| e.to_string())?;

    let target = format!("{ip}:{port}");
    let hello = crate::peerclient::hello(&target)
        .await
        .map_err(|e| format!("could not reach {target}: {e}"))?;

    if hello.device_id == config::load_config_impl(&app).device_id {
        return Err("that address is this device".to_string());
    }

    // Prefer the port the device reports over the one that was typed: someone
    // entering a bare IP gets the right port even when it is not the default.
    let port = if hello.port != 0 { hello.port } else { port };

    let peer = DiscoveredPeer {
        device_id: hello.device_id.clone(),
        name: crate::config::clean_device_name(&hello.name),
        platform: hello.platform,
        addresses: vec![ip.to_string()],
        port,
        last_seen_ms: utils::now_ms(),
        // Never evicted by the stale sweep: the user typed this in, so
        // forgetting it would be data loss rather than tidying.
        manual: true,
    };

    if let Ok(mut table) = state.discovered.lock() {
        // Merge rather than duplicate -- the typed address may be a second
        // interface of a device we already hear from.
        match table.get_mut(&hello.device_id) {
            Some(existing) => {
                existing.manual = true;
                existing.port = port;
                existing.last_seen_ms = peer.last_seen_ms;
                let addr = ip.to_string();
                existing.addresses.retain(|a| a != &addr);
                existing.addresses.insert(0, addr);
            }
            None => {
                table.insert(hello.device_id.clone(), peer.clone());
            }
        }
    }
    Ok(peer)
}

// ---------------------------------------------------------------------------
// Peers: pairing
// ---------------------------------------------------------------------------

/// Start pairing with a discovered device.
///
/// Long-running by nature -- it waits for a human on the far side -- so it
/// follows the existing `start_*_task` shape: the UI polls `get_task_progress`
/// and reads `pair_result` for the code and then the outcome.
#[tauri::command]
pub(crate) fn start_pair_task(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    device_id: String,
) -> Result<TaskHandle, String> {
    let target = {
        let table = state
            .discovered
            .lock()
            .map_err(|_| "discovery state poisoned".to_string())?;
        let peer = table
            .get(&device_id)
            .ok_or_else(|| "that device is no longer visible".to_string())?;
        let address = peer
            .best_address()
            .ok_or_else(|| "that device has no known address".to_string())?;
        format!("{}:{}", address, peer.port)
    };

    let task_id = alloc_task(&state, "pair_starting", "Contacting the device")?;

    let (my_id, my_name, my_port) = {
        let config = config::load_config_impl(&app);
        (config.device_id, config.device_name, config.port)
    };

    let tasks = Arc::clone(&state.tasks);
    let cancel = Arc::new(AtomicBool::new(false));
    if let Ok(mut cancels) = state.cancellations.lock() {
        cancels.insert(task_id, Arc::clone(&cancel));
    }
    let app_handle = app.clone();

    thread::spawn(move || {
        // A private current-thread runtime: this is not the server's thread,
        // and pairing is the only async work it will ever do.
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                finish_pair_task(&tasks, task_id, Err(err.to_string()));
                return;
            }
        };

        let outcome = runtime.block_on(async {
            let handshake = crate::peerclient::begin_pairing(&target, &my_id, &my_name, my_port)
                .await
                .map_err(|e| e.to_string())?;

            // Publish the code the instant it exists, so the user can start
            // walking to the other device while we wait.
            set_pair_code(&tasks, task_id, &handshake.code, &handshake.remote_name);

            let peer = crate::peerclient::await_pairing(&handshake, &cancel)
                .await
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((handshake.code.clone(), peer))
        });

        match outcome {
            Ok((code, peer)) => match store_peer(&app_handle, &peer) {
                Ok(()) => finish_pair_task(
                    &tasks,
                    task_id,
                    Ok(PairResult {
                        status: "accepted".to_string(),
                        code,
                        peer_id: peer.device_id.clone(),
                        peer_name: peer.name.clone(),
                        message: format!("Paired with {}", peer.name),
                    }),
                ),
                Err(err) => finish_pair_task(&tasks, task_id, Err(err)),
            },
            Err(text) => {
                // Map the failure onto a status the UI can phrase properly --
                // "declined" and "timed out" deserve different copy from a
                // genuine error.
                let status = if text.contains("declined") {
                    "declined"
                } else if text.contains("cancelled") {
                    "cancelled"
                } else if text.contains("in time") {
                    "expired"
                } else {
                    "error"
                };
                finish_pair_task(
                    &tasks,
                    task_id,
                    Ok(PairResult {
                        status: status.to_string(),
                        message: text,
                        ..Default::default()
                    }),
                );
            }
        }
    });

    Ok(TaskHandle { id: task_id })
}

/// Publish the pairing code onto the live task payload.
fn set_pair_code(
    tasks: &Arc<Mutex<HashMap<u64, ProgressPayload>>>,
    task_id: u64,
    code: &str,
    peer_name: &str,
) {
    if let Ok(mut guard) = tasks.lock() {
        if let Some(payload) = guard.get_mut(&task_id) {
            payload.phase = "pair_waiting".to_string();
            payload.message = format!("Waiting for {peer_name} to accept");
            payload.pair_result = Some(PairResult {
                status: "pending".to_string(),
                code: code.to_string(),
                peer_name: peer_name.to_string(),
                ..Default::default()
            });
        }
    }
}

fn finish_pair_task(
    tasks: &Arc<Mutex<HashMap<u64, ProgressPayload>>>,
    task_id: u64,
    result: Result<PairResult, String>,
) {
    if let Ok(mut guard) = tasks.lock() {
        if let Some(payload) = guard.get_mut(&task_id) {
            payload.done = true;
            payload.progress = 1.0;
            match result {
                Ok(res) => {
                    payload.phase = res.status.clone();
                    payload.message = res.message.clone();
                    payload.pair_result = Some(res);
                }
                Err(err) => {
                    payload.phase = "error".to_string();
                    payload.error = Some(err);
                }
            }
        }
    }
}

/// Add or replace a peer in the config. Both sides of a pairing land here.
pub(crate) fn store_peer(app: &tauri::AppHandle, peer: &Peer) -> Result<(), String> {
    let mut config = config::load_config_impl(app);
    config.peers.retain(|p| p.device_id != peer.device_id);
    config.peers.push(peer.clone());
    config::normalize(&mut config);
    config::save_config_impl(app, &config).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn list_incoming_pair_requests(state: State<'_, AppState>) -> Vec<PairPromptView> {
    crate::peers::list_prompts(&state)
}

#[tauri::command]
pub(crate) fn accept_pair_request(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    pair_id: String,
) -> Result<String, String> {
    let peer = crate::peers::accept_prompt(&state, &pair_id).map_err(|e| e.to_string())?;
    let name = peer.name.clone();
    mutate_config(&app, &state, |config| {
        config.peers.retain(|p| p.device_id != peer.device_id);
        config.peers.push(peer.clone());
        Ok(())
    })?;
    Ok(name)
}

#[tauri::command]
pub(crate) fn decline_pair_request(
    state: State<'_, AppState>,
    pair_id: String,
) -> Result<(), String> {
    crate::peers::decline_prompt(&state, &pair_id).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Peers: the friends list
// ---------------------------------------------------------------------------

/// When a peer was last known to be there, out of the three things that can
/// know it: its UDP beacon, the last direct authenticated contact either way,
/// and -- as a floor -- the moment it was paired.
///
/// The latest wins. Each is sufficient on its own and each has a blind spot:
/// the beacon does not survive a network that drops broadcast, contact says
/// nothing about a device sitting idle, and the pairing time is only ever a
/// starting point.
///
/// Pure and argument-taking, for the same reason `discovery::health_from` is:
/// `list_peers` needs it behind a `State` that a unit test cannot build, and a
/// rule that can only be exercised through a Tauri command is a rule that gets
/// tested by hand or not at all.
pub(crate) fn peer_seen_ms(beacon_ms: Option<u64>, contact_ms: Option<u64>, paired_ms: u64) -> u64 {
    paired_ms
        .max(beacon_ms.unwrap_or(0))
        .max(contact_ms.unwrap_or(0))
}

#[tauri::command]
pub(crate) fn list_peers(state: State<'_, AppState>) -> Vec<PeerView> {
    let now = utils::now_ms();
    let discovered = state.discovered.lock().ok();
    let contacted = state.peer_seen.lock().ok();

    state
        .peers
        .read()
        .map(|registry| {
            registry
                .peers
                .iter()
                .map(|p| {
                    // Beacon, last direct contact, pairing time -- latest wins.
                    // The contact half is what makes a device on a network that
                    // blocks broadcast read online after it talks to us, which
                    // this comment claimed long before anything wrote the
                    // timestamp it was claiming to read.
                    let found = discovered.as_ref().and_then(|t| t.get(&p.device_id));
                    let seen = peer_seen_ms(
                        found.map(|d| d.last_seen_ms),
                        contacted.as_ref().and_then(|t| t.get(&p.device_id)).copied(),
                        p.last_seen_ms,
                    );
                    let address = found
                        .and_then(|d| d.best_address().map(|a| format!("{}:{}", a, d.port)))
                        .or_else(|| p.last_address.clone());
                    PeerView {
                        device_id: p.device_id.clone(),
                        name: p.name.clone(),
                        platform: p.platform.clone(),
                        address,
                        last_seen_ms: seen,
                        online: now.saturating_sub(seen) <= crate::models::PEER_OFFLINE_AFTER_MS,
                        blocked: p.blocked,
                        added_ms: p.added_ms,
                        note: p.note.clone(),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tauri::command]
pub(crate) fn unpair_peer(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    device_id: String,
) -> Result<(), String> {
    mutate_config(&app, &state, |config| {
        let before = config.peers.len();
        config.peers.retain(|p| p.device_id != device_id);
        if config.peers.len() == before {
            return Err("that device is not paired".to_string());
        }
        Ok(())
    })
}

#[tauri::command]
pub(crate) fn rename_peer(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    device_id: String,
    name: String,
) -> Result<(), String> {
    mutate_config(&app, &state, |config| {
        let peer = config
            .peers
            .iter_mut()
            .find(|p| p.device_id == device_id)
            .ok_or_else(|| "that device is not paired".to_string())?;
        let cleaned = config::clean_device_name(&name);
        peer.name = if cleaned.is_empty() {
            device_id.clone()
        } else {
            cleaned
        };
        Ok(())
    })
}

#[tauri::command]
pub(crate) fn set_peer_blocked(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    device_id: String,
    blocked: bool,
) -> Result<(), String> {
    mutate_config(&app, &state, |config| {
        let peer = config
            .peers
            .iter_mut()
            .find(|p| p.device_id == device_id)
            .ok_or_else(|| "that device is not paired".to_string())?;
        peer.blocked = blocked;
        Ok(())
    })
}

/// List a paired device's shares, or one folder inside one.
///
/// This calls THEIR ordinary `/api/shares` and `/api/list` with the pair token.
/// Nothing peer-specific exists on the far side -- which is the whole point of
/// giving a peer a real session scope instead of a parallel API, because it
/// means the containment checks exercised by the browser path are the same code
/// protecting this one.
#[tauri::command]
pub(crate) async fn peer_browse(
    state: State<'_, AppState>,
    device_id: String,
    share_id: Option<String>,
    path: Option<String>,
) -> Result<serde_json::Value, String> {
    let (token, address) = peer_endpoint(&state, &device_id)?;

    let route = match share_id {
        Some(share) => format!(
            "/api/list?share={}&path={}",
            utils::url_encode(&share),
            utils::url_encode(path.as_deref().unwrap_or(""))
        ),
        None => "/api/shares".to_string(),
    };

    let answered = crate::peerclient::get_authed(&address, &route, &token)
        .await
        .map_err(|e| e.to_string())?;

    note_peer_seen(&state, &device_id);
    Ok(answered)
}

/// Play a file that lives on a paired device, without downloading it first.
///
/// Their machine mints the play link; ours hands it to a player. The player
/// never learns the pair token -- it gets a URL good for that one file.
#[tauri::command]
pub(crate) async fn play_peer_file(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    device_id: String,
    share_id: String,
    path: String,
) -> Result<String, String> {
    let (token, address) = peer_endpoint(&state, &device_id)?;

    let (url, name) = peer_play_url(&address, &token, &share_id, &path).await?;

    let player = config::load_config_impl(&app).external_player;
    if player.is_some() {
        launch_player(player.as_deref(), &url)?;
    } else {
        // No configured player, so we cannot hand over a bare URL: the OS would
        // route it to a browser, which is the one program that cannot play it.
        // A one-line playlist turns it into a file type every player claims.
        let playlist = write_play_playlist(&app, &name, &url)?;
        launch_player(None, &playlist)?;
    }
    Ok(url)
}

/// Mint a play link on the far device and build the URL it serves from.
///
/// Shared by "open in player" and by the Devices page's own preview, because
/// they want the same thing: a plain `http://` URL for one file, good for a few
/// hours, that something other than this app can fetch. See `models::PlayToken`.
async fn peer_play_url(
    address: &str,
    token: &str,
    share_id: &str,
    path: &str,
) -> Result<(String, String), String> {
    let link = crate::peerclient::play_link(address, token, share_id, path)
        .await
        .map_err(|e| e.to_string())?;
    let url = format!(
        "http://{address}/play/{}/{}",
        utils::url_encode_segment(&link.token),
        utils::url_encode_segment(&link.name)
    );
    Ok((url, link.name))
}

/// A URL the desktop UI can point an `<img>`, `<video>` or `<audio>` at.
///
/// The webview cannot attach our pair token to a subresource request, so it
/// gets a play link instead -- which means real `Range` support, and video that
/// seeks, rather than a base64 blob of the whole file.
#[tauri::command]
pub(crate) async fn peer_media_url(
    state: State<'_, AppState>,
    device_id: String,
    share_id: String,
    path: String,
) -> Result<String, String> {
    let (token, address) = peer_endpoint(&state, &device_id)?;
    let (url, _) = peer_play_url(&address, &token, &share_id, &path).await?;
    Ok(url)
}

/// A thumbnail from another device, as a `data:` URL.
///
/// Proxied rather than linked: a grid is hundreds of images, and minting a play
/// link per tile would churn through the token table for pictures measured in
/// kilobytes. The full-size preview goes the other way round -- see
/// `peer_media_url`.
#[tauri::command]
pub(crate) async fn peer_thumb(
    state: State<'_, AppState>,
    device_id: String,
    share_id: String,
    path: String,
) -> Result<String, String> {
    /// Generous for a thumbnail, small enough that a hostile answer cannot
    /// balloon this process.
    const MAX_THUMB_BYTES: usize = 2 * 1024 * 1024;

    let (token, address) = peer_endpoint(&state, &device_id)?;
    let route = format!(
        "/api/thumb?share={}&path={}",
        utils::url_encode(&share_id),
        utils::url_encode(&path)
    );

    let (bytes, mime) =
        crate::peerclient::get_authed_bytes(&address, &route, &token, MAX_THUMB_BYTES)
            .await
            .map_err(|e| e.to_string())?;

    Ok(format!("data:{};base64,{}", mime, utils::base64_encode(&bytes)))
}

// ---------------------------------------------------------------------------
// Network: downloading
// ---------------------------------------------------------------------------

/// One thing the user ticked: a file, or a folder to be walked.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct DownloadItem {
    pub(crate) path: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) is_dir: bool,
    #[serde(default)]
    pub(crate) size: u64,
}

/// A file to fetch, with where it goes relative to the destination.
struct Planned {
    /// Path on the other device, inside the share.
    remote: String,
    /// Path under the destination folder, or inside the archive.
    relative: String,
    size: u64,
}

/// Pull files from a paired device.
///
/// Zipping is the user's choice and not a consequence of how many things they
/// picked -- one file can be zipped, and forty can arrive as forty files in a
/// folder tree. The one shortcut: a single folder, zipped, streams the other
/// device's own `/zip` route, which already builds exactly this archive and
/// costs one request instead of one per file.
#[tauri::command]
pub(crate) fn start_peer_download_task(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    device_id: String,
    share_id: String,
    items: Vec<DownloadItem>,
    zip: bool,
) -> Result<TaskHandle, String> {
    if items.is_empty() {
        return Err("nothing selected".to_string());
    }
    let (token, address) = peer_endpoint(&state, &device_id)?;
    let dest_root = crate::peers::downloads_dir(&app).map_err(|e| e.to_string())?;

    let task_id = alloc_task(&state, "download_starting", "Preparing")?;
    let cancel = Arc::new(AtomicBool::new(false));
    if let Ok(mut cancels) = state.cancellations.lock() {
        cancels.insert(task_id, Arc::clone(&cancel));
    }
    let tasks = Arc::clone(&state.tasks);

    tauri::async_runtime::spawn(async move {
        let result =
            run_peer_download(&address, &token, &share_id, items, zip, &dest_root, &tasks, task_id, &cancel)
                .await;
        finish_download_task(&tasks, task_id, result);
    });

    Ok(TaskHandle { id: task_id })
}

async fn run_peer_download(
    address: &str,
    token: &str,
    share_id: &str,
    items: Vec<DownloadItem>,
    zip: bool,
    dest_root: &std::path::Path,
    tasks: &Arc<Mutex<HashMap<u64, ProgressPayload>>>,
    task_id: u64,
    cancel: &Arc<AtomicBool>,
) -> Result<DownloadResult, String> {
    // The whole-folder shortcut: their zip route already produces this archive.
    if zip && items.len() == 1 && items[0].is_dir {
        set_task_progress(tasks, task_id, "download_running", 0.0, "Requesting the archive");
        let route = format!(
            "/zip/{}/{}",
            utils::url_encode_segment(share_id),
            encode_remote_path(&items[0].path)
        );
        let target = utils::unique_destination(dest_root, &format!("{}.zip", items[0].name))
            .ok_or_else(|| "could not pick a name for the archive".to_string())?;
        let bytes =
            stream_to_path(address, &route, token, &target, tasks, task_id, cancel, 0, None).await?;
        return Ok(DownloadResult {
            path: shares::display_path(&target),
            file_count: 1,
            total_bytes: bytes,
            total_bytes_text: utils::format_bytes(bytes),
            zipped: true,
        });
    }

    // Otherwise: expand folders into their files, so a tree arrives as a tree.
    set_task_progress(tasks, task_id, "download_running", 0.0, "Listing files");
    let mut planned: Vec<Planned> = Vec::new();
    for item in &items {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".to_string());
        }
        if item.is_dir {
            expand_folder(address, token, share_id, &item.path, &item.name, &mut planned).await?;
        } else {
            planned.push(Planned {
                remote: item.path.clone(),
                relative: item.name.clone(),
                size: item.size,
            });
        }
    }
    if planned.is_empty() {
        return Err("nothing to download".to_string());
    }

    let total_expected: u64 = planned.iter().map(|p| p.size).sum();
    let file_count = planned.len() as u64;

    if zip {
        let name = if items.len() == 1 {
            format!("{}.zip", items[0].name)
        } else {
            "LAN Share selection.zip".to_string()
        };
        let target = utils::unique_destination(dest_root, &name)
            .ok_or_else(|| "could not pick a name for the archive".to_string())?;
        let bytes = zip_from_peer(
            address, token, share_id, &planned, &target, tasks, task_id, cancel, total_expected,
        )
        .await?;
        return Ok(DownloadResult {
            path: shares::display_path(&target),
            file_count,
            total_bytes: bytes,
            total_bytes_text: utils::format_bytes(bytes),
            zipped: true,
        });
    }

    // Separate files. Everything lands under one folder, so a multi-file pick
    // does not scatter across Downloads.
    let folder = if planned.len() == 1 && items.len() == 1 && !items[0].is_dir {
        dest_root.to_path_buf()
    } else {
        let label = if items.len() == 1 {
            items[0].name.clone()
        } else {
            "LAN Share".to_string()
        };
        let dir = utils::unique_destination(dest_root, &label)
            .ok_or_else(|| "could not pick a folder name".to_string())?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        dir
    };

    let mut done_bytes = 0u64;
    for (index, file) in planned.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".to_string());
        }
        let target = safe_join(&folder, &file.relative)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let target = utils::unique_destination(
            target.parent().unwrap_or(&folder),
            &target
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".to_string()),
        )
        .ok_or_else(|| "could not pick a file name".to_string())?;

        let route = format!(
            "/download/{}/{}",
            utils::url_encode_segment(share_id),
            encode_remote_path(&file.remote)
        );
        let label = format!("{} ({}/{})", file.relative, index + 1, planned.len());
        let bytes = stream_to_path(
            address,
            &route,
            token,
            &target,
            tasks,
            task_id,
            cancel,
            done_bytes,
            Some((total_expected, label)),
        )
        .await?;
        done_bytes += bytes;
    }

    Ok(DownloadResult {
        path: shares::display_path(&folder),
        file_count,
        total_bytes: done_bytes,
        total_bytes_text: utils::format_bytes(done_bytes),
        zipped: false,
    })
}

/// Walk a folder on the other device, depth-first, collecting its files.
///
/// Boxed because it recurses: an `async fn` that awaits itself has an infinitely
/// sized future otherwise.
fn expand_folder<'a>(
    address: &'a str,
    token: &'a str,
    share_id: &'a str,
    path: &'a str,
    prefix: &'a str,
    out: &'a mut Vec<Planned>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        let route = format!(
            "/api/list?share={}&path={}",
            utils::url_encode(share_id),
            utils::url_encode(path)
        );
        let listing = crate::peerclient::get_authed(address, &route, token)
            .await
            .map_err(|e| e.to_string())?;

        let entries = listing
            .get("entries")
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default();

        for entry in entries {
            let name = entry.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let child_remote = if path.is_empty() {
                name.to_string()
            } else {
                format!("{path}/{name}")
            };
            let child_relative = format!("{prefix}/{name}");

            if entry.get("is_dir").and_then(|d| d.as_bool()).unwrap_or(false) {
                expand_folder(address, token, share_id, &child_remote, &child_relative, out).await?;
            } else {
                out.push(Planned {
                    remote: child_remote,
                    relative: child_relative,
                    size: entry.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
                });
            }
        }
        Ok(())
    })
}

/// Stream one route into one file, reporting progress as it goes.
#[allow(clippy::too_many_arguments)]
async fn stream_to_path(
    address: &str,
    route: &str,
    token: &str,
    target: &std::path::Path,
    tasks: &Arc<Mutex<HashMap<u64, ProgressPayload>>>,
    task_id: u64,
    cancel: &Arc<AtomicBool>,
    already_done: u64,
    total: Option<(u64, String)>,
) -> Result<u64, String> {
    use std::io::Write as _;

    let file = std::fs::File::create(target)
        .map_err(|e| format!("could not write {}: {e}", target.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    let mut written = 0u64;
    let mut failure: Option<String> = None;

    let result = crate::peerclient::stream_authed(address, route, token, |chunk| {
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        if let Err(err) = writer.write_all(chunk) {
            failure = Some(err.to_string());
            return false;
        }
        written += chunk.len() as u64;
        report(tasks, task_id, already_done + written, total.as_ref());
        true
    })
    .await;

    let flushed = writer.flush();

    if let Some(err) = failure {
        let _ = std::fs::remove_file(target);
        return Err(err);
    }
    if let Err(err) = result {
        // A half-written file is worse than none: it looks like a download that
        // worked until you open it.
        let _ = std::fs::remove_file(target);
        return Err(err.to_string());
    }
    flushed.map_err(|e| e.to_string())?;
    Ok(written)
}

/// Build one archive locally out of several remote files.
#[allow(clippy::too_many_arguments)]
async fn zip_from_peer(
    address: &str,
    token: &str,
    share_id: &str,
    planned: &[Planned],
    target: &std::path::Path,
    tasks: &Arc<Mutex<HashMap<u64, ProgressPayload>>>,
    task_id: u64,
    cancel: &Arc<AtomicBool>,
    total_expected: u64,
) -> Result<u64, String> {
    let file = std::fs::File::create(target)
        .map_err(|e| format!("could not write {}: {e}", target.display()))?;
    let mut zip = crate::zipstream::ZipStream::new(std::io::BufWriter::new(file));
    let mut done_bytes = 0u64;

    for (index, item) in planned.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(target);
            return Err("cancelled".to_string());
        }
        let route = format!(
            "/download/{}/{}",
            utils::url_encode_segment(share_id),
            encode_remote_path(&item.remote)
        );
        let label = format!("{} ({}/{})", item.relative, index + 1, planned.len());

        let mut entry = zip
            .begin_file(&item.relative, item.size, utils::now_ms())
            .map_err(|e| e.to_string())?;
        let mut failure: Option<String> = None;

        let streamed = crate::peerclient::stream_authed(address, &route, token, |chunk| {
            if cancel.load(Ordering::SeqCst) {
                return false;
            }
            if let Err(err) = zip.write_chunk(&mut entry, chunk) {
                failure = Some(err.to_string());
                return false;
            }
            done_bytes += chunk.len() as u64;
            report(tasks, task_id, done_bytes, Some(&(total_expected, label.clone())));
            true
        })
        .await;

        if let Some(err) = failure {
            let _ = std::fs::remove_file(target);
            return Err(err);
        }
        if let Err(err) = streamed {
            let _ = std::fs::remove_file(target);
            return Err(err.to_string());
        }
        zip.end_file(entry).map_err(|e| e.to_string())?;
    }

    let mut writer = zip.finish().map_err(|e| e.to_string())?;
    {
        use std::io::Write as _;
        writer.flush().map_err(|e| e.to_string())?;
    }
    std::fs::metadata(target)
        .map(|m| m.len())
        .map_err(|e| e.to_string())
}

fn report(
    tasks: &Arc<Mutex<HashMap<u64, ProgressPayload>>>,
    task_id: u64,
    done: u64,
    total: Option<&(u64, String)>,
) {
    let (fraction, message) = match total {
        Some((expected, label)) if *expected > 0 => (
            done as f64 / *expected as f64,
            format!("{} · {}", label, utils::format_bytes(done)),
        ),
        // No declared total (their zip route is chunked with no length), so the
        // bar stays indeterminate and the byte count carries the signal.
        _ => (0.0, utils::format_bytes(done)),
    };
    set_task_progress(tasks, task_id, "download_running", fraction, message);
}

fn finish_download_task(
    tasks: &Arc<Mutex<HashMap<u64, ProgressPayload>>>,
    task_id: u64,
    result: Result<DownloadResult, String>,
) {
    if let Ok(mut guard) = tasks.lock() {
        if let Some(payload) = guard.get_mut(&task_id) {
            payload.done = true;
            payload.progress = 1.0;
            match result {
                Ok(done) => {
                    payload.phase = "done".to_string();
                    payload.message = format!(
                        "{} file{} · {}",
                        done.file_count,
                        if done.file_count == 1 { "" } else { "s" },
                        done.total_bytes_text
                    );
                    payload.download_result = Some(done);
                }
                Err(err) => {
                    payload.phase = "error".to_string();
                    payload.error = Some(err);
                }
            }
        }
    }
}

/// Percent-encode a path for a `/download/` or `/zip/` URL, segment by segment,
/// so a `/` stays a separator and everything else is escaped.
fn encode_remote_path(path: &str) -> String {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(utils::url_encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Join a relative path under `root` without letting it escape.
///
/// The names come from another machine's listing, so they are input: `..`, an
/// absolute path or a drive letter must not be able to steer a write out of the
/// downloads folder.
fn safe_join(root: &std::path::Path, relative: &str) -> Result<PathBuf, String> {
    let mut out = root.to_path_buf();
    for segment in relative.split('/') {
        let segment = segment.trim();
        if segment.is_empty() || segment == "." {
            continue;
        }
        // Reject, never sanitize -- the same rule the serving side applies,
        // used here because these names came off another machine's listing and
        // are therefore input.
        shares::check_segment(segment)
            .map_err(|_| format!("refusing a suspicious name: {relative}"))?;
        out.push(segment);
    }
    if out == root {
        return Err("empty name".to_string());
    }
    Ok(out)
}

/// Where to reach a peer: the live beacon first, then whatever address they
/// last contacted us from.
fn resolve_peer_address(state: &AppState, peer: &Peer) -> Option<String> {
    if let Ok(table) = state.discovered.lock() {
        if let Some(found) = table.get(&peer.device_id) {
            if let Some(address) = found.best_address() {
                return Some(format!("{}:{}", address, found.port));
            }
        }
    }
    peer.last_address.clone()
}

/// Record that we just reached a device, for `list_peers` to fold into presence.
///
/// Called on OUTBOUND success, where `auth.rs` records the inbound half. The
/// inbound stamp alone only fixes the view of whoever is being browsed; this is
/// the side that matters for the chicken-and-egg, because the desktop refuses
/// to call `peer_browse` at all on a device the peer list says is offline. With
/// no beacon and no outbound stamp, a reachable device stays unbrowsable
/// because it is unbrowsable.
///
/// `peer_browse` only. The thumbnail and download paths reach the same device
/// and could stamp too, but they are reachable only from a listing that
/// `peer_browse` just fetched, so they would be re-stamping a device stamped
/// milliseconds ago. Browsing is the coarse heartbeat; the rest is noise.
fn note_peer_seen(state: &AppState, device_id: &str) {
    if let Ok(mut seen) = state.peer_seen.lock() {
        seen.insert(device_id.to_string(), utils::now_ms());
    }
}

/// Resolve a paired device to something we can dial. Every Network command
/// starts here, so the blocked check cannot be forgotten in one of them.
fn peer_endpoint(state: &AppState, device_id: &str) -> Result<(String, String), String> {
    let registry = state
        .peers
        .read()
        .map_err(|_| "peer state poisoned".to_string())?;
    let peer = registry
        .by_id(device_id)
        .cloned()
        .ok_or_else(|| "that device is not paired".to_string())?;
    if peer.blocked {
        return Err("that device is blocked".to_string());
    }
    let address = resolve_peer_address(state, &peer)
        .ok_or_else(|| format!("{} is not reachable right now", peer.name))?;
    Ok((peer.out_token, address))
}

/// Write the one-line `.m3u` used to hand a peer's URL to the default player.
///
/// One fixed filename, overwritten each time: the player reads it the moment it
/// starts, so there is nothing to keep, and a per-play file would leave a
/// growing pile of dead playlists in the cache directory.
fn write_play_playlist(app: &tauri::AppHandle, name: &str, url: &str) -> Result<String, String> {
    use tauri::Manager as _;
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("no cache directory: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create the cache dir: {e}"))?;
    let file = dir.join("lanshare-play.m3u");
    let title = name.replace(['\r', '\n'], " ");
    std::fs::write(&file, format!("#EXTM3U\n#EXTINF:-1,{title}\n{url}\n"))
        .map_err(|e| format!("could not write the playlist: {e}"))?;
    Ok(file.display().to_string())
}
