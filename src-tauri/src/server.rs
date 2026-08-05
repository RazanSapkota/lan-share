//! Server lifecycle: binding, the runtime thread, router assembly, and
//! graceful shutdown. No request logic lives here.

use std::{
    net::{SocketAddr, TcpListener as StdListener},
    sync::{atomic::Ordering, mpsc, Arc},
    thread,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use crate::{
    discovery, media,
    models::{AppState, ServerCtx, ServerHandle, ServerStatus},
    peers, routes,
    utils::{format_bytes, now_ms},
};

const AUTO_PORT_TRIES: u16 = 10;
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const RUNTIME_DRAIN: Duration = Duration::from_secs(2);

/// Bind on the CALLING thread, so a failure is a plain `Err` from the Tauri
/// command rather than a message that arrives after the UI already said
/// "started". The std listener is handed to tokio afterwards.
fn bind_listener(bind_addr: &str, port: u16, auto_port: bool) -> Result<StdListener> {
    let ip: std::net::IpAddr = bind_addr
        .parse()
        .with_context(|| format!("invalid bind address {bind_addr:?}"))?;

    let tries = if auto_port { AUTO_PORT_TRIES } else { 1 };
    let mut last: Option<std::io::Error> = None;

    for offset in 0..tries {
        let candidate = port.saturating_add(offset);
        match StdListener::bind(SocketAddr::new(ip, candidate)) {
            Ok(listener) => {
                // REQUIRED before TcpListener::from_std, or the accept loop
                // blocks the runtime worker forever.
                listener.set_nonblocking(true)?;
                return Ok(listener);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => last = Some(err),
            Err(err) => {
                return Err(anyhow!("failed to bind {bind_addr}:{candidate}: {err}"));
            }
        }
    }

    let detail = last.map(|e| e.to_string()).unwrap_or_default();
    if auto_port {
        Err(anyhow!(
            "ports {}-{} are all in use ({detail}). Pick a different port in Settings.",
            port,
            port.saturating_add(tries - 1)
        ))
    } else {
        Err(anyhow!(
            "port {port} is already in use ({detail}). Pick a different port, or turn on Auto port in Settings."
        ))
    }
}

pub(crate) fn build_router(ctx: ServerCtx) -> Router {
    let max_upload = ctx
        .settings
        .read()
        .map(|s| s.max_upload_mb)
        .unwrap_or(4096)
        .saturating_mul(1024 * 1024) as usize;

    Router::new()
        // --- public shell + assets ---
        .route("/", get(routes::shell))
        .route("/assets/app.js", get(routes::asset_js))
        .route("/assets/styles.css", get(routes::asset_css))
        // The app mark: the tab icon, the PIN screen's mark, and the two sizes
        // the manifest lists.
        .route("/assets/icon-192.png", get(routes::asset_icon_192))
        .route("/assets/icon-512.png", get(routes::asset_icon_512))
        .route("/favicon.ico", get(routes::favicon))
        .route("/manifest.webmanifest", get(routes::manifest))
        // --- public api ---
        .route("/api/ping", get(routes::ping))
        .route("/api/session", get(routes::session_info))
        .route("/api/auth", post(routes::auth_pin))
        .route("/api/logout", post(routes::logout))
        .route("/s/{token}", get(routes::share_link))
        // --- peer api ---
        // `hello` and the three pairing routes are unauthenticated by
        // necessity: a device you have not paired with yet has no token. Each
        // is rate-limited and each returns the same 404 body on refusal.
        // Pairing, and nothing else. A paired device reads over the ordinary
        // routes below with its bearer token; there is deliberately no way for
        // one device to push anything at another.
        .route("/api/peer/hello", get(peers::hello))
        .route("/api/peer/pair/request", post(peers::pair_request))
        .route("/api/peer/pair/reveal", post(peers::pair_reveal))
        .route("/api/peer/pair/poll", post(peers::pair_poll))
        // --- authenticated api ---
        .route("/api/shares", get(routes::list_shares))
        .route("/api/list", get(routes::list_dir))
        .route("/api/thumb", get(routes::thumb))
        .route("/files/{share}/{*path}", get(routes::serve_inline))
        .route("/download/{share}/{*path}", get(routes::serve_attachment))
        // --- external players ---
        // The two GETs authenticate on the play token in the path and nothing
        // else, which is the only way a player -- which cannot hold a cookie --
        // gets past the PIN. The token names one file and expires; see
        // `models::PlayToken`. The trailing name is decorative, so both forms
        // are registered rather than relying on `{*name}` to match nothing.
        .route("/api/play/link", post(routes::play_link))
        .route("/play/{token}", get(routes::play_stream))
        .route("/play/{token}/{*name}", get(routes::play_stream_named))
        .route("/playlist/{token}", get(routes::play_playlist))
        // Both root forms are registered explicitly: `{*path}` does not match
        // an empty segment, so `/zip/{share}` and `/zip/{share}/` would
        // otherwise fall through to the SPA fallback and return HTML.
        .route("/zip/{share}", get(routes::serve_zip_root))
        .route("/zip/{share}/", get(routes::serve_zip_root))
        .route("/zip/{share}/{*path}", get(routes::serve_zip))
        .route(
            "/api/upload",
            // axum's default 2 MiB cap would reject every real upload, so the
            // limit is raised to the configured maximum instead of removed.
            // (`DefaultBodyLimit::max` is used rather than tower-http's
            // `RequestBodyLimitLayer` because the latter rewrites the body
            // error type, which `Multipart` then cannot extract from.)
            post(routes::upload).layer(DefaultBodyLimit::max(max_upload)),
        )
        .fallback(routes::fallback)
        // Runs on EVERY request, before routing: DNS-rebinding guard.
        .layer(axum::middleware::from_fn_with_state(
            ctx.clone(),
            routes::host_guard,
        ))
        .with_state(ctx)
    // Deliberately NO CorsLayer: same-origin only is the security model.
}

pub(crate) fn start_server_impl(app: &tauri::AppHandle, state: &AppState) -> Result<ServerStatus> {
    {
        let guard = state
            .server
            .lock()
            .map_err(|_| anyhow!("server state poisoned"))?;
        if guard.is_some() {
            return Err(anyhow!("server is already running"));
        }
    }

    let (bind_addr, port, auto_port, keep_awake, discovery_port, peering) = {
        let s = state
            .settings
            .read()
            .map_err(|_| anyhow!("settings poisoned"))?;
        (
            s.bind_address.clone(),
            s.port,
            s.auto_port,
            s.keep_awake,
            s.discovery_port,
            s.peering_enabled,
        )
    };

    let listener = bind_listener(&bind_addr, port, auto_port)?;
    let addr = listener.local_addr()?;

    // Bound HERE, beside the TCP listener, for two reasons. The first is the
    // same one `bind_listener` has: a failure becomes a plain Err from the
    // Tauri command instead of a silent one inside a spawned thread. The second
    // is specific to Windows -- this is what raises the SECOND firewall prompt,
    // because the prompt is per (program, PROTOCOL, profile) and allowing TCP
    // does nothing for UDP. Binding lazily on the first announce would surface
    // a mystery dialog five seconds after the user clicked Start, with nothing
    // on screen to connect it to.
    //
    // Bound whenever peering is on, which -- since the server running IS the
    // consent to be seen -- means whenever the server runs.
    let discovery_socket = if peering {
        match discovery::bind_receiver(discovery_port) {
            Ok(sock) => {
                if let Ok(mut slot) = state.discovery_error.lock() {
                    *slot = None;
                }
                Some(sock)
            }
            Err(err) => {
                // Discovery is a convenience; sharing files is not. A blocked
                // UDP port must not stop the server from starting.
                if let Ok(mut slot) = state.discovery_error.lock() {
                    *slot = Some(err.to_string());
                }
                None
            }
        }
    } else {
        None
    };
    state
        .discovery_bound
        .store(discovery_socket.is_some(), Ordering::Relaxed);

    let thumb_dir = media::thumb_dir(app)?;
    if let Ok(mut slot) = state.thumb_dir.lock() {
        *slot = Some(thumb_dir.clone());
    }

    let device_id = {
        let config = crate::config::load_config_impl(app);
        Arc::new(config.device_id)
    };

    let ctx = ServerCtx {
        settings: Arc::clone(&state.settings),
        shares: Arc::clone(&state.shares),
        sessions: Arc::clone(&state.sessions),
        play_tokens: Arc::clone(&state.play_tokens),
        pin_attempts: Arc::clone(&state.pin_attempts),
        activity: Arc::clone(&state.activity),
        next_activity_id: Arc::clone(&state.next_activity_id),
        bytes_served: Arc::clone(&state.bytes_served),
        requests_served: Arc::clone(&state.requests_served),
        thumb_permits: Arc::clone(&state.thumb_permits),
        thumb_dir,
        peers: Arc::clone(&state.peers),
        discovered: Arc::clone(&state.discovered),
        peer_seen: Arc::clone(&state.peer_seen),
        pending_pairs: Arc::clone(&state.pending_pairs),
        pair_attempts: Arc::clone(&state.pair_attempts),
        device_id,
        discovery_self_seen_ms: Arc::clone(&state.discovery_self_seen_ms),
        discovery_started_ms: Arc::clone(&state.discovery_started_ms),
    };

    // `watch` rather than `oneshot`: three tasks now share one shutdown signal,
    // and a oneshot receiver cannot be cloned.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (finished_tx, finished_rx) = mpsc::channel::<()>();

    let thread = thread::Builder::new()
        .name("lanshare-http".into())
        .spawn(move || {
            // SetThreadExecutionState is THREAD-AFFINE, so it has to be set
            // from the thread that stays alive for the server's lifetime --
            // not from the Tauri command thread that returns immediately.
            #[cfg(windows)]
            if keep_awake {
                keep_awake_on();
            }
            #[cfg(not(windows))]
            let _ = keep_awake;

            // Two workers is plenty: tokio::fs dispatches the actual reads to
            // the blocking pool, so the workers only shuffle buffers.
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .max_blocking_threads(16)
                .thread_name("lanshare-rt")
                .enable_io()
                .enable_time()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => {
                    let _ = finished_tx.send(());
                    return;
                }
            };

            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(l) => l,
                    Err(_) => return,
                };

                let http = {
                    let mut rx = shutdown_rx.clone();
                    let router = build_router(ctx.clone());
                    // ConnectInfo gives handlers the peer IP, which the
                    // activity log, the PIN rate limiter and the pairing
                    // IP-binding all need.
                    let service = router.into_make_service_with_connect_info::<SocketAddr>();
                    async move {
                        let _ = axum::serve(listener, service)
                            .with_graceful_shutdown(async move {
                                // Ignore the initial `false`; wake on the flip.
                                let _ = rx.wait_for(|stop| *stop).await;
                            })
                            .await;
                    }
                };

                // `join!`, not `select!`: the HTTP server must be allowed to
                // drain in-flight downloads after the signal, and select! would
                // drop the other futures the moment one finished.
                // Registered with the reactor here, not at bind time: the bind
                // has to happen on the calling thread (for the error path and
                // the firewall prompt), but `from_std` panics without a
                // running runtime.
                match discovery_socket.and_then(|s| discovery::adopt(s).ok()) {
                    Some(socket) => {
                        let sock = Arc::new(socket);
                        tokio::join!(
                            http,
                            discovery::announce_loop(ctx.clone(), Arc::clone(&sock), shutdown_rx.clone()),
                            discovery::receive_loop(ctx.clone(), sock, shutdown_rx.clone()),
                            crate::peers::sweep_loop(ctx.clone(), shutdown_rx.clone()),
                        );
                    }
                    None => {
                        tokio::join!(
                            http,
                            crate::peers::sweep_loop(ctx.clone(), shutdown_rx.clone()),
                        );
                    }
                }
            });

            // Do not wait forever for a 4 GB download to drain on exit.
            runtime.shutdown_timeout(RUNTIME_DRAIN);

            #[cfg(windows)]
            if keep_awake {
                keep_awake_off();
            }

            let _ = finished_tx.send(());
        })
        .context("failed to spawn the HTTP server thread")?;

    let handle = ServerHandle {
        addr,
        shutdown: Some(shutdown_tx),
        finished: finished_rx,
        thread: Some(thread),
        started_at_ms: now_ms(),
    };
    *state
        .server
        .lock()
        .map_err(|_| anyhow!("server state poisoned"))? = Some(handle);

    Ok(status_impl(state))
}

pub(crate) fn stop_server_impl(state: &AppState) -> Result<ServerStatus> {
    let handle = state
        .server
        .lock()
        .map_err(|_| anyhow!("server state poisoned"))?
        .take();

    if let Some(mut handle) = handle {
        // Flip the watch, then drop the sender. Sending is what wakes
        // `wait_for`; dropping alone would only make the receivers error out,
        // which the announce loop would see as a spurious wake.
        if let Some(tx) = handle.shutdown.take() {
            let _ = tx.send(true);
        }

        // Timed join: `JoinHandle` has no `join_timeout`, so the runtime thread
        // signals a plain mpsc channel just before returning. Past the deadline
        // we abandon it -- `shutdown_timeout` has already force-dropped the
        // tasks, and on app exit the process dies anyway.
        match handle.finished.recv_timeout(STOP_TIMEOUT) {
            Ok(()) => {
                if let Some(t) = handle.thread.take() {
                    let _ = t.join();
                }
            }
            Err(_) => { /* abandoned; sockets close with the process */ }
        }
    }

    // The discovery socket went with the runtime thread.
    state.discovery_bound.store(false, Ordering::Relaxed);
    state.discovery_started_ms.store(0, Ordering::Relaxed);

    // Sessions are per-server-run: stopping the server logs everyone out. Play
    // links go with them -- a link handed to a player is no more durable than
    // the session that minted it.
    if let Ok(mut sessions) = state.sessions.lock() {
        sessions.clear();
    }
    if let Ok(mut play) = state.play_tokens.lock() {
        play.clear();
    }
    if let Ok(mut attempts) = state.pin_attempts.lock() {
        attempts.clear();
    }

    Ok(status_impl(state))
}

pub(crate) fn restart_server_impl(
    app: &tauri::AppHandle,
    state: &AppState,
) -> Result<ServerStatus> {
    let _ = stop_server_impl(state);
    start_server_impl(app, state)
}

pub(crate) fn is_running(state: &AppState) -> bool {
    state
        .server
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
}

pub(crate) fn status_impl(state: &AppState) -> ServerStatus {
    let (running, port, started_at_ms) = match state.server.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(handle) => (true, handle.addr.port(), handle.started_at_ms),
            None => (false, 0, 0),
        },
        Err(_) => (false, 0, 0),
    };

    let settings = state.settings.read().ok();
    let (bind_address, pin_enabled, pin, uploads_enabled, configured_port) = settings
        .as_ref()
        .map(|s| {
            (
                s.bind_address.clone(),
                s.pin_enabled,
                s.pin.clone(),
                s.uploads_enabled,
                s.port,
            )
        })
        .unwrap_or_else(|| ("0.0.0.0".to_string(), true, String::new(), false, 0));

    let (share_count, enabled_share_count) = state
        .shares
        .read()
        .map(|r| {
            (
                r.shares.len(),
                r.shares.iter().filter(|s| s.cfg.enabled).count(),
            )
        })
        .unwrap_or((0, 0));

    let bytes_served = state.bytes_served.load(Ordering::Relaxed);

    ServerStatus {
        running,
        // Report the port actually bound while running, the configured one
        // otherwise -- auto-port means those can differ.
        port: if running { port } else { configured_port },
        bind_address,
        started_at_ms,
        uptime_ms: if running {
            now_ms().saturating_sub(started_at_ms)
        } else {
            0
        },
        bytes_served,
        bytes_served_text: format_bytes(bytes_served),
        requests_served: state.requests_served.load(Ordering::Relaxed),
        session_count: state.sessions.lock().map(|s| s.len()).unwrap_or(0),
        share_count,
        enabled_share_count,
        pin_enabled,
        pin,
        uploads_enabled,
    }
}

// ---------------------------------------------------------------------------
// Keep-awake (Windows)
// ---------------------------------------------------------------------------

/// Keep the SYSTEM awake during transfers. `ES_DISPLAY_REQUIRED` is
/// deliberately omitted -- the screen may still sleep, which is what a user
/// sharing files from a closed-lid laptop actually wants.
#[cfg(windows)]
fn keep_awake_on() {
    use windows::Win32::System::Power::{
        SetThreadExecutionState, ES_AWAYMODE_REQUIRED, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
    };
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_AWAYMODE_REQUIRED);
    }
}

#[cfg(windows)]
fn keep_awake_off() {
    use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS};
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS);
    }
}
