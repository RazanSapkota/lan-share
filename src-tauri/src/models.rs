//! Every shared type: `AppState`, `AppConfig` and its `Default`, the HTTP-side
//! settings/registry snapshots, session types, activity entries, and the
//! task/progress payloads. Structural sibling of the reference app's
//! `models.rs` -- same `Arc<Mutex<..>>` + `AtomicU64` idiom for state, same
//! hand-written `Default` + per-field `#[serde(default)]` idiom for config.

use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc, Mutex, RwLock,
    },
};

use serde::{Deserialize, Serialize};

use crate::net;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub(crate) const CONFIG_FILE_NAME: &str = "lan_share.config.json";
pub(crate) const THUMB_DIR_NAME: &str = "thumbs";
pub(crate) const DEFAULT_LANGUAGE: &str = "en_US";
pub(crate) const DEFAULT_PORT: u16 = 8080;
pub(crate) const ACTIVITY_LOG_CAP: usize = 2000;
pub(crate) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Session cookie name. Deliberately app-specific so two of these running on
/// one machine (different ports, same host) don't clobber each other -- cookies
/// ignore the port component of an origin.
pub(crate) const COOKIE_NAME: &str = "lanshare_sid";

/// Header the receiver UI sends on every mutating request. A cross-site form
/// post cannot set it, so its presence proves the request came from our own JS.
pub(crate) const CSRF_HEADER: &str = "x-lanshare";

// --- peers -----------------------------------------------------------------

pub(crate) const DEFAULT_DISCOVERY_PORT: u16 = 57321;
/// Link-local scope: routers do not forward it off the subnet, which is
/// exactly the reach we want.
pub(crate) const DISCOVERY_MULTICAST: [u8; 4] = [239, 255, 90, 17];
pub(crate) const BEACON_MAGIC: &str = "LANSHARE/1";
/// Bounded so a forged packet cannot make us allocate.
pub(crate) const BEACON_MAX_BYTES: usize = 512;
pub(crate) const ANNOUNCE_EVERY_MS: u64 = 5_000;
/// Three missed announces. Short enough to feel live, long enough to survive a
/// single dropped datagram on a busy Wi-Fi.
pub(crate) const PEER_OFFLINE_AFTER_MS: u64 = 20_000;
pub(crate) const PEER_EVICT_AFTER_MS: u64 = 300_000;
/// A forged-id flood must not grow the table without bound.
pub(crate) const DISCOVERED_CAP: usize = 256;
pub(crate) const DEVICE_NAME_MAX: usize = 48;

// --- play links ------------------------------------------------------------

/// How long an external-player link stays valid.
///
/// Six hours, because the thing on the other end is a film: a 30-minute token
/// dies while you are still watching it, and one that never expires is a
/// permanent hole punched through the PIN for whoever the link reached.
pub(crate) const PLAY_TOKEN_TTL_MS: u64 = 6 * 3600 * 1000;
/// Live links, at most. Evicts the soonest-to-expire rather than refusing a new
/// one -- being unable to play the file you just tapped is a worse failure than
/// cutting short a link somebody probably finished with.
pub(crate) const PLAY_TOKEN_CAP: usize = 64;

pub(crate) const PAIR_TTL_MS: u64 = 120_000;
pub(crate) const PAIR_MAX_ATTEMPTS: u32 = 5;
pub(crate) const PAIR_LOCKOUT_SECONDS: u32 = 60;

// ---------------------------------------------------------------------------
// Task machinery -- copied wholesale from the reference app
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskHandle {
    pub(crate) id: u64,
}

/// `Default` lets every construction site list only the fields it cares about
/// and finish with `..Default::default()`, so adding a new `*_result` variant
/// is a one-line change here instead of an edit at every site.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ProgressPayload {
    pub(crate) phase: String,
    pub(crate) progress: f64,
    pub(crate) message: String,
    pub(crate) done: bool,
    pub(crate) error: Option<String>,
    #[serde(default)]
    pub(crate) index_result: Option<IndexShareResponse>,
    #[serde(default)]
    pub(crate) prewarm_result: Option<PrewarmResponse>,
    #[serde(default)]
    pub(crate) pair_result: Option<PairResult>,
    /// Where a peer download landed, so the page can offer to reveal it.
    #[serde(default)]
    pub(crate) download_result: Option<DownloadResult>,
}

/// The finished shape of a peer download.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct DownloadResult {
    /// Folder the files went into, or the archive itself when zipped.
    pub(crate) path: String,
    pub(crate) file_count: u64,
    pub(crate) total_bytes: u64,
    pub(crate) total_bytes_text: String,
    pub(crate) zipped: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct IndexShareResponse {
    pub(crate) share_id: String,
    pub(crate) file_count: u64,
    pub(crate) dir_count: u64,
    pub(crate) total_bytes: u64,
    pub(crate) total_bytes_text: String,
    /// Directories that could not be read (permissions, disconnected drive).
    pub(crate) skipped: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PrewarmResponse {
    pub(crate) share_id: String,
    pub(crate) generated: u64,
    pub(crate) reused: u64,
    pub(crate) failed: u64,
}

// ---------------------------------------------------------------------------
// Shares
// ---------------------------------------------------------------------------

/// One shared folder or file.
///
/// `id` appears in every URL and must never change once handed out. `token` is
/// the per-share secret link. Neither is derived from the path -- a share whose
/// folder is moved on disk keeps its links working after the user re-points it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Share {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: String,
    /// Per-share secret link token. Empty on configs written before this
    /// existed; load/add backfills one.
    #[serde(default)]
    pub(crate) token: String,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    /// The single drop-box folder. Enforced to at most one across all shares.
    #[serde(default)]
    pub(crate) is_inbox: bool,
    /// Uploads are refused when true even if this share is the inbox.
    #[serde(default = "default_true")]
    pub(crate) read_only: bool,
    /// A single file added via `pick_files` rather than a folder.
    #[serde(default)]
    pub(crate) is_file: bool,
    /// false => only the top level is browsable; subfolders are hidden.
    #[serde(default = "default_true")]
    pub(crate) recursive: bool,
    /// Exact filenames to expose, case-insensitively. Empty = everything.
    ///
    /// What "Add files" uses: picking three photos out of a folder roots the
    /// share at their parent and then restricts it to exactly those three,
    /// rather than exposing the folder they happen to live in.
    #[serde(default)]
    pub(crate) include_names: Vec<String>,
    /// Lowercase, no dot. Empty = everything.
    #[serde(default)]
    pub(crate) include_ext: Vec<String>,
    #[serde(default)]
    pub(crate) exclude_ext: Vec<String>,
    #[serde(default)]
    pub(crate) added_ms: u64,
    #[serde(default)]
    pub(crate) note: Option<String>,
}

/// A `Share` with its root canonicalized. NOT persisted: a canonical path can
/// change (drive remounted, folder moved) and a stale one on disk is worse than
/// none. Rebuilt by `config::apply_config_to_state` on every mutation.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedShare {
    pub(crate) cfg: Share,
    /// `std::fs::canonicalize` output -- verbatim `\\?\` form on Windows.
    pub(crate) root: PathBuf,
    pub(crate) root_exists: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ShareRegistry {
    pub(crate) shares: Vec<ResolvedShare>,
}

impl ShareRegistry {
    pub(crate) fn all(&self) -> impl Iterator<Item = &ResolvedShare> {
        self.shares.iter()
    }

    pub(crate) fn by_id(&self, id: &str) -> Option<&ResolvedShare> {
        self.all().find(|s| s.cfg.id == id)
    }

    /// Token lookup for `/s/{token}`. Disabled shares are invisible, so
    /// toggling a share off kills its secret link too.
    pub(crate) fn by_token(&self, token: &str) -> Option<&ResolvedShare> {
        if token.is_empty() {
            return None;
        }
        self.all()
            .find(|s| s.cfg.enabled && crate::auth::ct_eq_str(&s.cfg.token, token))
    }
}

/// What the desktop Shares table renders. `Share` plus derived, non-persisted
/// display fields.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShareView {
    #[serde(flatten)]
    pub(crate) cfg: Share,
    pub(crate) root_exists: bool,
    /// `dunce::simplified` form -- the `\\?\` prefix confuses users and Explorer.
    pub(crate) display_path: String,
    pub(crate) link: Option<String>,
}

// ---------------------------------------------------------------------------
// Peers
// ---------------------------------------------------------------------------

/// A paired device. Persisted -- this is the friends list.
///
/// Two tokens, not one shared secret. `in_token` is what the remote presents to
/// US (we issued it, we can revoke it alone); `out_token` is what we present to
/// THEM (they issued it). Keeping them separate means a token leaked from one
/// side cannot be replayed back at its issuer, and either direction can be cut
/// without re-pairing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Peer {
    pub(crate) device_id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) platform: String,
    pub(crate) in_token: String,
    pub(crate) out_token: String,
    #[serde(default)]
    pub(crate) added_ms: u64,
    #[serde(default)]
    pub(crate) last_seen_ms: u64,
    /// Last address we successfully reached them on, so browsing still works
    /// when the beacon has not arrived yet after a restart.
    #[serde(default)]
    pub(crate) last_address: Option<String>,
    /// Refused everywhere, but kept in the list so the block is undoable.
    #[serde(default)]
    pub(crate) blocked: bool,
    #[serde(default)]
    pub(crate) note: Option<String>,
}

/// Token-indexed view of the paired devices, rebuilt by
/// `config::apply_config_to_state` on every mutation -- the same live-state
/// property the PIN and the share registry have.
#[derive(Debug, Clone, Default)]
pub(crate) struct PeerRegistry {
    pub(crate) peers: Vec<Peer>,
}

impl PeerRegistry {
    pub(crate) fn by_id(&self, id: &str) -> Option<&Peer> {
        self.peers.iter().find(|p| p.device_id == id)
    }

    /// Resolve an inbound bearer token. Constant-time, and an empty token never
    /// matches -- a hand-edited config with a blank token must not become a
    /// skeleton key.
    pub(crate) fn by_in_token(&self, token: &str) -> Option<&Peer> {
        if token.is_empty() {
            return None;
        }
        self.peers
            .iter()
            .find(|p| crate::auth::ct_eq_str(&p.in_token, token))
    }
}

/// What the desktop Devices page renders for a paired device. `Peer` minus the
/// tokens -- those must never reach the UI layer -- plus derived presence.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PeerView {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) platform: String,
    pub(crate) address: Option<String>,
    pub(crate) last_seen_ms: u64,
    pub(crate) online: bool,
    pub(crate) blocked: bool,
    pub(crate) added_ms: u64,
    pub(crate) note: Option<String>,
}

/// A device heard on the LAN. In-memory only: presence is not state worth
/// persisting, and a stale "seen yesterday" row is worse than an empty list.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DiscoveredPeer {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) platform: String,
    /// Every address we have heard this device on. A machine with Wi-Fi and
    /// Ethernet on the same subnet announces from both.
    pub(crate) addresses: Vec<String>,
    pub(crate) port: u16,
    pub(crate) last_seen_ms: u64,
    /// Typed in by hand rather than heard. Never evicted by the stale sweep.
    #[serde(default)]
    pub(crate) manual: bool,
}

impl DiscoveredPeer {
    pub(crate) fn online(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_seen_ms) <= PEER_OFFLINE_AFTER_MS
    }

    pub(crate) fn best_address(&self) -> Option<&str> {
        self.addresses.first().map(|s| s.as_str())
    }
}

/// Where a half-finished pairing has got to. Lives on the RESPONDER.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PairState {
    /// The initiator has committed to its nonce but not revealed it, so we
    /// cannot compute the code yet and must show the user nothing.
    Committed,
    /// Nonce revealed, code computed, the user is looking at the prompt.
    Waiting,
    Accepted,
    Declined,
}

/// A pairing in flight, held by the responder until the user answers.
#[derive(Debug, Clone)]
pub(crate) struct PendingPair {
    pub(crate) pair_id: String,
    pub(crate) from_id: String,
    pub(crate) from_name: String,
    pub(crate) from_platform: String,
    pub(crate) from_ip: String,
    pub(crate) from_port: u16,
    /// H("commit" || nonce_a), sent before we reveal nonce_b.
    pub(crate) commit: String,
    pub(crate) nonce_b: String,
    /// The token the initiator issued for us to use when talking to them.
    pub(crate) out_token: String,
    /// Filled once the nonce is revealed and the code can be derived.
    pub(crate) code: Option<String>,
    pub(crate) state: PairState,
    pub(crate) expires_ms: u64,
    /// Set on accept; handed to the initiator by the single-use poll.
    pub(crate) issued_in_token: Option<String>,
}

/// Desktop-facing incoming pair prompt.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PairPromptView {
    pub(crate) pair_id: String,
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) platform: String,
    pub(crate) address: String,
    pub(crate) code: String,
    pub(crate) expires_ms: u64,
}

/// What the initiating side polls for.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PairResult {
    /// pending | accepted | declined | expired | cancelled | error
    pub(crate) status: String,
    pub(crate) code: String,
    pub(crate) peer_id: String,
    pub(crate) peer_name: String,
    pub(crate) message: String,
}

// ---------------------------------------------------------------------------
// Device identity and discovery status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DeviceIdentity {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) platform: String,
    pub(crate) addresses: Vec<String>,
    pub(crate) peering_enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct DiscoveryStatus {
    /// We are announcing, so other devices can see this one. Running the server
    /// is the whole of the decision: there is no second switch.
    pub(crate) running: bool,
    /// The UDP socket is bound, so we can hear other devices.
    pub(crate) listening: bool,
    pub(crate) port: u16,
    /// ok | inbound_likely_blocked | nothing_heard | error
    pub(crate) health: String,
    pub(crate) error: Option<String>,
    pub(crate) devices: Vec<DiscoveredPeerView>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DiscoveredPeerView {
    #[serde(flatten)]
    pub(crate) peer: DiscoveredPeer,
    pub(crate) online: bool,
    pub(crate) paired: bool,
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionScope {
    /// PIN-authenticated: sees every enabled share.
    Full,
    /// Entered via `/s/{token}`: sees exactly one share. The token is stored so
    /// regenerating the share's token instantly invalidates this session.
    Share { share_id: String, token: String },
    /// A paired device presenting its `in_token`. Sees every enabled share, the
    /// same as `Full` -- pairing is a deliberate, code-confirmed act, so a peer
    /// is at least as trusted as someone who typed the PIN.
    ///
    /// This is NOT stored in the session map: a peer authenticates by bearer
    /// token on every request, so there is no session to expire or revoke.
    /// Unpairing removes the token, which is the revocation.
    Peer { peer_id: String },
}

#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub(crate) scope: SessionScope,
    pub(crate) created_ms: u64,
    pub(crate) last_seen_ms: u64,
    pub(crate) client_ip: String,
    pub(crate) user_agent: String,
}

impl Session {
    pub(crate) fn share_id(&self) -> Option<&str> {
        match &self.scope {
            SessionScope::Full | SessionScope::Peer { .. } => None,
            SessionScope::Share { share_id, .. } => Some(share_id.as_str()),
        }
    }

    pub(crate) fn scope_name(&self) -> &'static str {
        match &self.scope {
            SessionScope::Full => "full",
            SessionScope::Share { .. } => "share",
            SessionScope::Peer { .. } => "peer",
        }
    }
}

/// Desktop-facing session row. The token is truncated -- the desktop needs to
/// identify a session to revoke it, not to impersonate it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SessionView {
    pub(crate) token: String,
    pub(crate) scope: String,
    pub(crate) share_id: Option<String>,
    pub(crate) client_ip: String,
    pub(crate) user_agent: String,
    pub(crate) created_ms: u64,
    pub(crate) last_seen_ms: u64,
}

/// A link handed to an external player (VLC and friends).
///
/// Deliberately NOT a session. A player cannot send our cookie, so the
/// credential has to ride in the URL -- which is exactly what the session
/// design refused, and for good reason. This is the narrow version of it: one
/// file, six hours, accepted only on `/play/*` and `/playlist/*`, so a link
/// pasted into a group chat leaks the film someone was already watching rather
/// than the run of the whole share.
#[derive(Debug, Clone)]
pub(crate) struct PlayToken {
    pub(crate) share_id: String,
    /// Path within the share, exactly as minted. Re-resolved on every fetch, so
    /// a share switched off mid-film stops the stream.
    pub(crate) rel: String,
    /// For the playlist title and the cosmetic tail of the URL -- players guess
    /// the format from the extension they see.
    pub(crate) file_name: String,
    pub(crate) expires_ms: u64,
}

/// Failed-PIN backoff, per client IP.
#[derive(Debug, Clone, Default)]
pub(crate) struct AttemptState {
    pub(crate) failures: u32,
    pub(crate) locked_until_ms: u64,
}

// ---------------------------------------------------------------------------
// Activity log
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ActivityEntry {
    pub(crate) id: u64,
    pub(crate) at_ms: u64,
    pub(crate) client_ip: String,
    pub(crate) user_agent: String,
    /// "auth" | "auth_failed" | "list" | "view" | "download" | "upload"
    /// | "thumb" | "denied"
    pub(crate) kind: String,
    pub(crate) share_id: Option<String>,
    pub(crate) share_name: Option<String>,
    pub(crate) path: Option<String>,
    /// Bytes actually written to the socket.
    pub(crate) bytes: u64,
    /// File size, so the UI can render a ratio for an aborted transfer.
    pub(crate) total_bytes: u64,
    /// "started" | "ok" | "aborted" | "error"
    pub(crate) status: String,
    pub(crate) detail: Option<String>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub(crate) struct ServerHandle {
    pub(crate) addr: SocketAddr,
    /// Flipped to `true` to shut every task down. A `watch` rather than a
    /// `oneshot` because the HTTP server, both discovery loops and the sweep
    /// task all wait on the same signal, and a oneshot receiver is not
    /// clonable.
    pub(crate) shutdown: Option<tokio::sync::watch::Sender<bool>>,
    /// The runtime thread signals here just before it returns, letting
    /// `stop_server` do a *timed* wait -- `JoinHandle` has no `join_timeout`.
    pub(crate) finished: std::sync::mpsc::Receiver<()>,
    pub(crate) thread: Option<std::thread::JoinHandle<()>>,
    pub(crate) started_at_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ServerStatus {
    pub(crate) running: bool,
    pub(crate) port: u16,
    pub(crate) bind_address: String,
    pub(crate) started_at_ms: u64,
    pub(crate) uptime_ms: u64,
    pub(crate) bytes_served: u64,
    pub(crate) bytes_served_text: String,
    pub(crate) requests_served: u64,
    pub(crate) session_count: usize,
    pub(crate) share_count: usize,
    pub(crate) enabled_share_count: usize,
    pub(crate) pin_enabled: bool,
    pub(crate) pin: String,
    pub(crate) uploads_enabled: bool,
}

/// The subset of `AppConfig` the HTTP handlers read, behind its own `RwLock` so
/// a request never contends with desktop-only fields.
#[derive(Debug, Clone, Default)]
pub(crate) struct ServerSettings {
    pub(crate) port: u16,
    pub(crate) bind_address: String,
    pub(crate) auto_port: bool,
    pub(crate) server_name: String,
    pub(crate) strict_host_check: bool,
    pub(crate) keep_awake: bool,
    pub(crate) pin_enabled: bool,
    pub(crate) pin: String,
    pub(crate) session_ttl_hours: u32,
    pub(crate) max_pin_attempts: u32,
    pub(crate) lockout_seconds: u32,
    pub(crate) uploads_enabled: bool,
    pub(crate) max_upload_mb: u64,
    pub(crate) upload_allowed_ext: Vec<String>,
    pub(crate) upload_dedupe_names: bool,
    pub(crate) thumbnails_enabled: bool,
    pub(crate) thumb_max_edge: u32,
    pub(crate) thumb_quality: u8,
    pub(crate) show_hidden: bool,
    pub(crate) hidden_names: Vec<String>,
    pub(crate) allow_folder_zip: bool,
    pub(crate) default_sort: String,
    pub(crate) default_sort_asc: bool,
    pub(crate) default_view_mode: String,
    // --- peers ---
    pub(crate) device_name: String,
    pub(crate) peering_enabled: bool,
    pub(crate) peer_browse_enabled: bool,
    pub(crate) discovery_port: u16,
}

impl ServerSettings {
    /// Only bind parameters require tearing the listener down. PIN, shares,
    /// inbox, thumbnails and everything else are read live off the `RwLock` by
    /// the handlers, so they take effect on the next request.
    pub(crate) fn needs_rebind(&self, other: &ServerSettings) -> bool {
        self.port != other.port
            || self.bind_address != other.bind_address
            // The discovery socket is bound alongside the TCP listener, so
            // changing its port needs the same teardown.
            || self.discovery_port != other.discovery_port
            // Peering is what decides whether that socket exists at all.
            || self.peering_enabled != other.peering_enabled
    }

    pub(crate) fn from_config(config: &AppConfig) -> Self {
        Self {
            port: config.port,
            bind_address: config.bind_address.clone(),
            auto_port: config.auto_port,
            server_name: config.server_name.clone(),
            strict_host_check: config.strict_host_check,
            keep_awake: config.keep_awake,
            pin_enabled: config.pin_enabled,
            pin: config.pin.clone(),
            session_ttl_hours: config.session_ttl_hours,
            max_pin_attempts: config.max_pin_attempts,
            lockout_seconds: config.lockout_seconds,
            uploads_enabled: config.uploads_enabled,
            max_upload_mb: config.max_upload_mb,
            upload_allowed_ext: config.upload_allowed_ext.clone(),
            upload_dedupe_names: config.upload_dedupe_names,
            thumbnails_enabled: config.thumbnails_enabled,
            thumb_max_edge: config.thumb_max_edge,
            thumb_quality: config.thumb_quality,
            show_hidden: config.show_hidden,
            hidden_names: config.hidden_names.clone(),
            allow_folder_zip: config.allow_folder_zip,
            default_sort: config.default_sort.clone(),
            default_sort_asc: config.default_sort_asc,
            default_view_mode: config.default_view_mode.clone(),
            device_name: config.device_name.clone(),
            peering_enabled: config.peering_enabled,
            peer_browse_enabled: config.peer_browse_enabled,
            discovery_port: config.discovery_port,
        }
    }
}

/// Everything an axum handler can see. Cloned `Arc`s of `AppState` fields, so a
/// mutation from a Tauri command is visible to the next HTTP request with no
/// restart and no message passing.
#[derive(Clone)]
pub(crate) struct ServerCtx {
    pub(crate) settings: Arc<RwLock<ServerSettings>>,
    pub(crate) shares: Arc<RwLock<ShareRegistry>>,
    pub(crate) sessions: Arc<Mutex<HashMap<String, Session>>>,
    /// Live external-player links. In memory beside the sessions, and gone for
    /// the same reason when the server stops.
    pub(crate) play_tokens: Arc<Mutex<HashMap<String, PlayToken>>>,
    pub(crate) pin_attempts: Arc<Mutex<HashMap<IpAddr, AttemptState>>>,
    pub(crate) activity: Arc<Mutex<VecDeque<ActivityEntry>>>,
    pub(crate) next_activity_id: Arc<AtomicU64>,
    pub(crate) bytes_served: Arc<AtomicU64>,
    pub(crate) requests_served: Arc<AtomicU64>,
    pub(crate) thumb_permits: Arc<tokio::sync::Semaphore>,
    pub(crate) thumb_dir: PathBuf,

    // --- peers ---
    pub(crate) peers: Arc<RwLock<PeerRegistry>>,
    pub(crate) discovered: Arc<Mutex<HashMap<String, DiscoveredPeer>>>,
    pub(crate) pending_pairs: Arc<Mutex<HashMap<String, PendingPair>>>,
    pub(crate) pair_attempts: Arc<Mutex<HashMap<IpAddr, AttemptState>>>,
    /// Our own identity, so handlers can answer `/api/peer/hello` and the
    /// beacon loop can announce without touching the config file.
    pub(crate) device_id: Arc<String>,
    /// Epoch-ms we last saw our OWN looped beacon. The inbound-path health
    /// check reads this; see `discovery::health`.
    pub(crate) discovery_self_seen_ms: Arc<AtomicU64>,
    pub(crate) discovery_started_ms: Arc<AtomicU64>,
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LanAddress {
    pub(crate) ip: String,
    pub(crate) interface: String,
    /// The address the OS would actually route from -- preselect this one.
    pub(crate) is_primary: bool,
    /// Hyper-V / WSL / VirtualBox / Docker adapters: real, but useless to a phone.
    pub(crate) is_virtual: bool,
    pub(crate) is_private: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LanUrl {
    pub(crate) label: String,
    pub(crate) ip: String,
    pub(crate) url: String,
    pub(crate) is_primary: bool,
    pub(crate) is_virtual: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct QrPayload {
    pub(crate) url: String,
    pub(crate) svg: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct FirewallHint {
    pub(crate) platform: String,
    pub(crate) command: String,
    pub(crate) note: String,
}

// ---------------------------------------------------------------------------
// Listing (shared between the HTTP API and the desktop Preview command)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ListingEntry {
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) is_symlink: bool,
    pub(crate) size: u64,
    pub(crate) size_text: String,
    pub(crate) mtime_ms: u64,
    /// image | video | audio | pdf | text | archive | other | dir
    pub(crate) kind: String,
    pub(crate) ext: String,
    /// None when no thumbnail can exist for this entry. The receiver cannot
    /// derive this, which is why it stays on the wire while the file/download
    /// URLs do not -- those are `share` + `path` and the client builds them.
    pub(crate) thumb_url: Option<String>,
    /// kind is video/audio/image AND the extension is browser-decodable.
    /// `.mkv`/`.avi`/`.wmv`/`.heic` are media but no browser plays them, and a
    /// broken <video> element is worse than an honest download card.
    pub(crate) playable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Breadcrumb {
    pub(crate) name: String,
    pub(crate) path: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ListingResponse {
    pub(crate) share_id: String,
    pub(crate) share_name: String,
    pub(crate) is_inbox: bool,
    pub(crate) can_upload: bool,
    pub(crate) path: String,
    pub(crate) parent: Option<String>,
    pub(crate) breadcrumbs: Vec<Breadcrumb>,
    pub(crate) entries: Vec<ListingEntry>,
    pub(crate) dir_count: usize,
    pub(crate) file_count: usize,
    pub(crate) total_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ThumbCacheStats {
    pub(crate) file_count: u64,
    pub(crate) total_bytes: u64,
    pub(crate) total_bytes_text: String,
    pub(crate) path: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct SaveConfigResult {
    pub(crate) rebound: bool,
    pub(crate) port: u16,
    pub(crate) warning: Option<String>,
}

// ---------------------------------------------------------------------------
// AppConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AppConfig {
    // --- app chrome ---
    pub(crate) language: String,
    pub(crate) theme: String,

    // --- server ---
    #[serde(default = "default_port")]
    pub(crate) port: u16,
    /// "0.0.0.0" reaches every interface. Binding one NIC breaks on every DHCP
    /// renewal and every Wi-Fi switch; the security model is the PIN and the
    /// share tokens, not the bind address. "127.0.0.1" is offered for testing.
    #[serde(default = "default_bind_address")]
    pub(crate) bind_address: String,
    /// Try `port..port+9` when the chosen port is taken. On by default: the URL
    /// and QR always show the ACTUAL port, so nobody types it from memory.
    #[serde(default = "default_true")]
    pub(crate) auto_port: bool,
    /// OFF by default. The first bind to 0.0.0.0 raises the Windows Firewall
    /// prompt; that must happen when the user clicks Start -- a moment they
    /// have context for -- not silently at launch.
    #[serde(default)]
    pub(crate) autostart_server: bool,
    #[serde(default = "default_server_name")]
    pub(crate) server_name: String,
    /// `SetThreadExecutionState` so the laptop does not suspend mid-transfer.
    #[serde(default = "default_true")]
    pub(crate) keep_awake: bool,
    /// Host-header allowlist. DNS-rebinding defense; leave on.
    #[serde(default = "default_true")]
    pub(crate) strict_host_check: bool,

    // --- auth ---
    #[serde(default = "default_true")]
    pub(crate) pin_enabled: bool,
    /// Cleartext, deliberately: the desktop app must DISPLAY the PIN, so it is
    /// by definition recoverable and hashing would be security theatre.
    /// Empty = generated on first start.
    #[serde(default)]
    pub(crate) pin: String,
    #[serde(default = "default_session_hours")]
    pub(crate) session_ttl_hours: u32,
    #[serde(default = "default_max_pin_attempts")]
    pub(crate) max_pin_attempts: u32,
    #[serde(default = "default_lockout_seconds")]
    pub(crate) lockout_seconds: u32,

    // --- shares ---
    #[serde(default)]
    pub(crate) shares: Vec<Share>,

    // --- uploads (off by default) ---
    #[serde(default)]
    pub(crate) uploads_enabled: bool,
    #[serde(default)]
    pub(crate) inbox_share_id: Option<String>,
    #[serde(default = "default_max_upload_mb")]
    pub(crate) max_upload_mb: u64,
    /// Lowercase, no dot. Empty = accept anything.
    #[serde(default)]
    pub(crate) upload_allowed_ext: Vec<String>,
    /// true => "photo (2).jpg"; false => refuse an existing name. Never
    /// overwrite -- a receiver must not be able to clobber the host's file.
    #[serde(default = "default_true")]
    pub(crate) upload_dedupe_names: bool,

    // --- media / listing ---
    #[serde(default = "default_true")]
    pub(crate) thumbnails_enabled: bool,
    #[serde(default = "default_thumb_max_edge")]
    pub(crate) thumb_max_edge: u32,
    #[serde(default = "default_thumb_quality")]
    pub(crate) thumb_quality: u8,
    #[serde(default = "default_thumb_cache_mb")]
    pub(crate) thumb_cache_max_mb: u64,
    /// Path to a media player to hand files to (VLC, mpv, whatever). Empty =
    /// use whatever the OS has registered for the file type.
    #[serde(default)]
    pub(crate) external_player: Option<String>,
    #[serde(default)]
    pub(crate) show_hidden: bool,
    /// Case-insensitive exact names always hidden, on top of the OS hidden bit.
    #[serde(default = "default_hidden_names")]
    pub(crate) hidden_names: Vec<String>,
    #[serde(default = "default_true")]
    pub(crate) allow_folder_zip: bool,
    #[serde(default = "default_sort")]
    pub(crate) default_sort: String,
    #[serde(default = "default_true")]
    pub(crate) default_sort_asc: bool,
    #[serde(default = "default_view_mode")]
    pub(crate) default_view_mode: String,

    // --- this device (peer identity) ---
    /// Stable across restarts, backfilled once by `normalize`. It appears in
    /// every beacon and in the pairing hash, so it must never be regenerated
    /// once a peer has stored it -- that would silently break every pairing.
    #[serde(default)]
    pub(crate) device_id: String,
    #[serde(default = "default_server_name")]
    pub(crate) device_name: String,
    #[serde(default = "default_true")]
    pub(crate) peering_enabled: bool,
    /// Whether a paired device may also browse and pull our shares.
    #[serde(default = "default_true")]
    pub(crate) peer_browse_enabled: bool,
    #[serde(default = "default_discovery_port")]
    pub(crate) discovery_port: u16,

    // --- paired devices (the friends list) ---
    #[serde(default)]
    pub(crate) peers: Vec<Peer>,

    // --- discovery ---
    #[serde(default)]
    pub(crate) mdns_enabled: bool,
    /// Pin the URL/QR to one adapter on a multi-homed machine.
    #[serde(default)]
    pub(crate) preferred_interface: Option<String>,

    // --- desktop UI state ---
    #[serde(default)]
    pub(crate) last_picked_dir: Option<String>,
    #[serde(default = "default_true")]
    pub(crate) minimize_to_tray: bool,
}

fn default_true() -> bool {
    true
}
fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_bind_address() -> String {
    "0.0.0.0".to_string()
}
fn default_server_name() -> String {
    net::hostname().unwrap_or_else(|| "LAN Share".to_string())
}
fn default_session_hours() -> u32 {
    12
}
fn default_max_pin_attempts() -> u32 {
    5
}
fn default_lockout_seconds() -> u32 {
    30
}
fn default_max_upload_mb() -> u64 {
    4096
}
fn default_thumb_max_edge() -> u32 {
    320
}
fn default_thumb_quality() -> u8 {
    78
}
fn default_thumb_cache_mb() -> u64 {
    512
}
fn default_sort() -> String {
    "name".to_string()
}
fn default_view_mode() -> String {
    "grid".to_string()
}
fn default_discovery_port() -> u16 {
    DEFAULT_DISCOVERY_PORT
}
fn default_hidden_names() -> Vec<String> {
    [
        "desktop.ini",
        "thumbs.db",
        ".ds_store",
        "system volume information",
        "$recycle.bin",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            language: DEFAULT_LANGUAGE.to_string(),
            theme: "dark".to_string(),
            port: DEFAULT_PORT,
            bind_address: default_bind_address(),
            auto_port: true,
            autostart_server: false,
            server_name: default_server_name(),
            keep_awake: true,
            strict_host_check: true,
            pin_enabled: true,
            pin: String::new(),
            session_ttl_hours: default_session_hours(),
            max_pin_attempts: default_max_pin_attempts(),
            lockout_seconds: default_lockout_seconds(),
            shares: Vec::new(),
            uploads_enabled: false,
            inbox_share_id: None,
            max_upload_mb: default_max_upload_mb(),
            upload_allowed_ext: Vec::new(),
            upload_dedupe_names: true,
            thumbnails_enabled: true,
            thumb_max_edge: default_thumb_max_edge(),
            thumb_quality: default_thumb_quality(),
            thumb_cache_max_mb: default_thumb_cache_mb(),
            external_player: None,
            show_hidden: false,
            hidden_names: default_hidden_names(),
            allow_folder_zip: true,
            default_sort: default_sort(),
            default_sort_asc: true,
            default_view_mode: default_view_mode(),
            // Left empty on purpose: `normalize` backfills it once, so a
            // freshly constructed default and a loaded config take the same
            // path and there is no way to mint two ids for one install.
            device_id: String::new(),
            device_name: default_server_name(),
            peering_enabled: true,
            peer_browse_enabled: true,
            discovery_port: DEFAULT_DISCOVERY_PORT,
            peers: Vec::new(),
            mdns_enabled: false,
            preferred_interface: None,
            last_picked_dir: None,
            minimize_to_tray: true,
        }
    }
}

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

pub(crate) struct AppState {
    // --- task machinery, identical to the reference app ---
    pub(crate) next_task_id: AtomicU64,
    pub(crate) tasks: Arc<Mutex<HashMap<u64, ProgressPayload>>>,
    /// Per-task cancel flags. Tasks that opt in register an `AtomicBool` here
    /// at start; the matching `cancel_task` command flips it. Spawned threads
    /// poll the flag at safe points and bail out gracefully.
    pub(crate) cancellations: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,

    /// The running server, or None. `start_server` fills it, `stop_server`
    /// takes it. Behind a Mutex so the two commands cannot race.
    pub(crate) server: Arc<Mutex<Option<ServerHandle>>>,

    /// Live server-visible settings. Handlers read this on every request, so a
    /// PIN change / share add / inbox toggle takes effect WITHOUT a restart.
    /// `RwLock` rather than `Mutex` because reads dominate massively and a
    /// Mutex would serialize every request behind every other.
    pub(crate) settings: Arc<RwLock<ServerSettings>>,

    /// Shares with their roots canonicalized. Derived from `AppConfig`, rebuilt
    /// by `config::apply_config_to_state` on every mutation.
    pub(crate) shares: Arc<RwLock<ShareRegistry>>,

    /// Browser sessions, keyed by opaque token.
    pub(crate) sessions: Arc<Mutex<HashMap<String, Session>>>,

    /// Links minted for external players. Same lifetime as the sessions: a
    /// stopped server invalidates every one of them.
    pub(crate) play_tokens: Arc<Mutex<HashMap<String, PlayToken>>>,

    /// Failed-PIN backoff, keyed by client IP.
    pub(crate) pin_attempts: Arc<Mutex<HashMap<IpAddr, AttemptState>>>,

    /// In-memory transfer/activity log, newest first, capped at
    /// `ACTIVITY_LOG_CAP`. Lost on quit -- by design; there is no database.
    pub(crate) activity: Arc<Mutex<VecDeque<ActivityEntry>>>,
    pub(crate) next_activity_id: Arc<AtomicU64>,

    /// Lifetime counters for the desktop status card.
    pub(crate) bytes_served: Arc<AtomicU64>,
    pub(crate) requests_served: Arc<AtomicU64>,

    /// Bounds concurrent image decodes. Each can hold a full-resolution bitmap.
    pub(crate) thumb_permits: Arc<tokio::sync::Semaphore>,

    /// `app_cache_dir()/thumbs`, resolved once in `setup()`.
    pub(crate) thumb_dir: Arc<Mutex<Option<PathBuf>>>,

    // --- peers ---
    /// Paired devices, token-indexed. Rebuilt on every config mutation, so
    /// unpairing takes effect on the next request with no restart.
    pub(crate) peers: Arc<RwLock<PeerRegistry>>,
    /// Heard on the LAN. Cleared when the server stops -- presence is not a
    /// fact that outlives the socket.
    pub(crate) discovered: Arc<Mutex<HashMap<String, DiscoveredPeer>>>,
    pub(crate) pending_pairs: Arc<Mutex<HashMap<String, PendingPair>>>,
    pub(crate) pair_attempts: Arc<Mutex<HashMap<IpAddr, AttemptState>>>,
    pub(crate) discovery_self_seen_ms: Arc<AtomicU64>,
    pub(crate) discovery_started_ms: Arc<AtomicU64>,
    pub(crate) discovery_error: Arc<Mutex<Option<String>>>,
    /// Whether the discovery socket is actually bound right now.
    ///
    /// False when the bind failed -- discovery is a convenience and a blocked
    /// UDP port does not stop the server, so the two can disagree.
    pub(crate) discovery_bound: Arc<AtomicBool>,
}

impl AppState {
    pub(crate) fn new() -> Self {
        // 2..=6 concurrent decodes. `image::Limits` caps each one's allocation,
        // so this bounds peak memory at a few hundred MB in the worst case.
        let permits = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1).clamp(2, 6))
            .unwrap_or(2);
        Self {
            next_task_id: AtomicU64::new(0),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            server: Arc::new(Mutex::new(None)),
            settings: Arc::new(RwLock::new(ServerSettings::default())),
            shares: Arc::new(RwLock::new(ShareRegistry::default())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            play_tokens: Arc::new(Mutex::new(HashMap::new())),
            pin_attempts: Arc::new(Mutex::new(HashMap::new())),
            activity: Arc::new(Mutex::new(VecDeque::with_capacity(256))),
            next_activity_id: Arc::new(AtomicU64::new(0)),
            bytes_served: Arc::new(AtomicU64::new(0)),
            requests_served: Arc::new(AtomicU64::new(0)),
            thumb_permits: Arc::new(tokio::sync::Semaphore::new(permits)),
            thumb_dir: Arc::new(Mutex::new(None)),
            peers: Arc::new(RwLock::new(PeerRegistry::default())),
            discovered: Arc::new(Mutex::new(HashMap::new())),
            pending_pairs: Arc::new(Mutex::new(HashMap::new())),
            pair_attempts: Arc::new(Mutex::new(HashMap::new())),
            discovery_self_seen_ms: Arc::new(AtomicU64::new(0)),
            discovery_started_ms: Arc::new(AtomicU64::new(0)),
            discovery_error: Arc::new(Mutex::new(None)),
            discovery_bound: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// The OS-reported platform, used for the device-list icon. Kept coarse on
/// purpose -- it is a hint for a glyph, not telemetry.
pub(crate) fn platform_name() -> String {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        "linux" => "linux",
        other => other,
    }
    .to_string()
}
