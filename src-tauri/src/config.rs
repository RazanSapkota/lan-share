//! Config persistence -- the reference app's `config.rs` pattern, with the
//! addition of `apply_config_to_state`, which rebuilds the two derived
//! snapshots (`ServerSettings`, `ShareRegistry`) the HTTP handlers read.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use tauri::Manager;

use crate::{
    models::{AppConfig, AppState, ResolvedShare, ServerSettings, Share, ShareRegistry,
             CONFIG_FILE_NAME},
    shares,
};

fn config_path(app: &tauri::AppHandle) -> Result<PathBuf> {
    let dir = app
        .path()
        .app_config_dir()
        .context("failed to resolve app config dir")?;
    fs::create_dir_all(&dir).context("failed to create app config dir")?;
    Ok(dir.join(CONFIG_FILE_NAME))
}

pub(crate) fn load_config_impl(app: &tauri::AppHandle) -> AppConfig {
    let path = match config_path(app) {
        Ok(path) => path,
        Err(_) => return AppConfig::default(),
    };

    let mut config = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<AppConfig>(&text).unwrap_or_default(),
        Err(_) => AppConfig::default(),
    };
    normalize(&mut config);
    config
}

pub(crate) fn save_config_impl(app: &tauri::AppHandle, config: &AppConfig) -> Result<()> {
    let path = config_path(app)?;
    fs::write(path, serde_json::to_vec_pretty(config)?).context("failed to write config")
}

/// Repair invariants that a hand-edited or older config could violate.
///
/// Runs on every load and before every save, so the rest of the code can assume
/// these hold rather than re-checking at each use site.
pub(crate) fn normalize(config: &mut AppConfig) {
    // Backfilled exactly once and then never touched. Every paired device
    // stores this id and it is an input to the pairing hash, so regenerating it
    // would silently break every existing pairing with no error anywhere.
    if config.device_id.trim().is_empty() {
        config.device_id = crate::auth::random_id();
    }
    config.device_name = clean_device_name(&config.device_name);
    if config.device_name.is_empty() {
        config.device_name = crate::net::hostname().unwrap_or_else(|| "LAN Share".to_string());
    }

    // A peer needs both tokens to be usable in either direction. A half-written
    // entry (interrupted pairing, hand-edited file) can only produce confusing
    // one-way failures, so drop it and let the user re-pair.
    config
        .peers
        .retain(|p| !p.device_id.trim().is_empty() && !p.in_token.is_empty() && !p.out_token.is_empty());

    // Keep the newest pairing for any duplicated device id: re-pairing an
    // existing device must replace it, not shadow it with an entry that
    // `by_id` may or may not return.
    config.peers.sort_by(|a, b| b.added_ms.cmp(&a.added_ms));
    {
        let mut seen = std::collections::HashSet::new();
        config.peers.retain(|p| seen.insert(p.device_id.clone()));
    }
    for peer in &mut config.peers {
        peer.name = clean_device_name(&peer.name);
        if peer.name.is_empty() {
            peer.name = peer.device_id.clone();
        }
    }

    // 0 and the privileged range are never valid; colliding with the HTTP port
    // is legal for UDP but confusing to explain, so it is refused too.
    if config.discovery_port < 1024 || config.discovery_port == config.port {
        config.discovery_port = crate::models::DEFAULT_DISCOVERY_PORT;
    }
    config.max_offer_files = config.max_offer_files.clamp(1, 10_000);
    config.max_offer_bytes = config.max_offer_bytes.max(1);

    normalize_inner(config);
}

/// Strip anything that would let a remote device's name mangle the UI: C0/C1
/// controls, and the bidi overrides that make `photo\u{202E}gnp.exe` render as
/// `photoexe.png`. Escaping alone does not stop those -- they are legal
/// characters that reorder what the eye sees.
pub(crate) fn clean_device_name(name: &str) -> String {
    name.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    c,
                    '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}'
                )
        })
        .take(crate::models::DEVICE_NAME_MAX)
        .collect::<String>()
        .trim()
        .to_string()
}

fn normalize_inner(config: &mut AppConfig) {
    // Every share needs a token: one written before tokens existed, or one
    // hand-edited to `""`, would otherwise be matchable by an empty token.
    // (`ShareRegistry::by_token` also rejects empty input -- belt and braces.)
    for share in &mut config.shares {
        if share.token.trim().is_empty() {
            share.token = crate::auth::random_token();
        }
        if share.id.trim().is_empty() {
            share.id = crate::auth::random_id();
        }
    }

    // At most one inbox. If several claim it, the one named by
    // `inbox_share_id` wins; otherwise the first.
    let claimed: Vec<String> = config
        .shares
        .iter()
        .filter(|s| s.is_inbox)
        .map(|s| s.id.clone())
        .collect();
    let winner = config
        .inbox_share_id
        .clone()
        .filter(|id| claimed.iter().any(|c| c == id))
        .or_else(|| claimed.first().cloned());
    for share in &mut config.shares {
        share.is_inbox = Some(&share.id) == winner.as_ref();
    }
    config.inbox_share_id = winner;

    // An inbox that is still read_only can never accept an upload; that
    // combination is a config that silently does nothing, so drop the flag.
    if let Some(inbox_id) = config.inbox_share_id.clone() {
        if !config.uploads_enabled {
            // Uploads globally off: keep the inbox designation (so toggling
            // uploads back on restores it) but nothing can be written.
        } else if let Some(share) = config.shares.iter_mut().find(|s| s.id == inbox_id) {
            share.read_only = false;
        }
    }

    // Clamp values a hand-edit could put out of range.
    config.thumb_max_edge = config.thumb_max_edge.clamp(64, 1024);
    config.thumb_quality = config.thumb_quality.clamp(30, 95);
    config.max_pin_attempts = config.max_pin_attempts.clamp(1, 100);
    config.lockout_seconds = config.lockout_seconds.clamp(1, 86_400);
    config.session_ttl_hours = config.session_ttl_hours.clamp(1, 24 * 30);
    config.max_upload_mb = config.max_upload_mb.clamp(1, 1024 * 1024);
    if config.port == 0 {
        config.port = crate::models::DEFAULT_PORT;
    }

    // Extension lists are compared lowercase, dotless.
    normalize_ext_list(&mut config.upload_allowed_ext);
    for share in &mut config.shares {
        normalize_ext_list(&mut share.include_ext);
        normalize_ext_list(&mut share.exclude_ext);
    }
    for name in &mut config.hidden_names {
        *name = name.trim().to_ascii_lowercase();
    }
    config.hidden_names.retain(|n| !n.is_empty());
}

fn normalize_ext_list(list: &mut Vec<String>) {
    for ext in list.iter_mut() {
        *ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    }
    list.retain(|e| !e.is_empty());
    list.sort();
    list.dedup();
}

/// Rebuild the derived snapshots the HTTP handlers read. Must be called after
/// every config mutation -- that is what makes a share add or PIN change take
/// effect on the next request with no server restart.
pub(crate) fn apply_config_to_state(state: &AppState, config: &AppConfig) -> Result<()> {
    let registry = build_registry(&config.shares);

    {
        let mut guard = state
            .settings
            .write()
            .map_err(|_| anyhow::anyhow!("settings state poisoned"))?;
        *guard = ServerSettings::from_config(config);
    }
    {
        let mut guard = state
            .shares
            .write()
            .map_err(|_| anyhow::anyhow!("share state poisoned"))?;
        // Carry the handoffs across. They are not in the config, so a blind
        // overwrite would silently kill a "Send to phone" link the moment any
        // unrelated setting changed.
        let ephemeral = std::mem::take(&mut guard.ephemeral);
        *guard = registry;
        guard.ephemeral = ephemeral;
        guard.sweep_ephemeral(crate::utils::now_ms());
    }
    {
        // Same live-state property the PIN has: unpairing or blocking a device
        // takes effect on its very next request, with no restart.
        let mut guard = state
            .peers
            .write()
            .map_err(|_| anyhow::anyhow!("peer state poisoned"))?;
        *guard = crate::models::PeerRegistry {
            peers: config.peers.clone(),
        };
    }

    // A share whose token changed (or that was removed) must not leave live
    // sessions holding the old one.
    prune_stale_sessions(state, config);
    Ok(())
}

pub(crate) fn build_registry(shares: &[Share]) -> ShareRegistry {
    let resolved = shares
        .iter()
        .map(|cfg| match shares::canonical_root(&cfg.path) {
            Ok(root) => ResolvedShare {
                cfg: cfg.clone(),
                root,
                root_exists: true,
                expires_ms: None,
            },
            // A disconnected drive or deleted folder must not drop the share
            // from the config -- the user would lose the name, token and
            // settings. Keep it, mark it unreachable, and refuse to serve it.
            Err(_) => ResolvedShare {
                cfg: cfg.clone(),
                root: PathBuf::from(&cfg.path),
                root_exists: false,
                expires_ms: None,
            },
        })
        .collect();
    ShareRegistry {
        shares: resolved,
        ephemeral: Vec::new(),
    }
}

fn prune_stale_sessions(state: &AppState, config: &AppConfig) {
    let Ok(mut sessions) = state.sessions.lock() else {
        return;
    };
    sessions.retain(|_, session| match &session.scope {
        crate::models::SessionScope::Full => true,
        // A peer never occupies a session slot -- it authenticates by bearer
        // token on every request -- so if one ever appears here it is stale.
        crate::models::SessionScope::Peer { .. } => false,
        crate::models::SessionScope::Share { share_id, token } => config
            .shares
            .iter()
            .any(|s| s.enabled && &s.id == share_id && &s.token == token),
    });
}
