#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod activity;
mod assets;
mod auth;
mod config;
mod discovery;
mod media;
mod models;
mod net;
mod peerclient;
mod peers;
mod routes;
mod server;
mod shares;
mod tasks;
#[cfg(test)]
mod tests;
mod utils;
mod zipstream;

use tauri::Manager;

use crate::{
    models::AppState,
    // Devices: pairing and the friends list. Nothing pushes files any more --
    // everything moves because the other side asked for it.
    tasks::{
        accept_pair_request, add_peer_by_address, decline_pair_request, get_device_identity,
        list_discovered, list_incoming_pair_requests, list_peers, rename_peer, set_device_name,
        set_peer_blocked, start_pair_task, unpair_peer,
    },
    // Network: reading another device's shares.
    tasks::{
        peer_browse, peer_media_url, peer_thumb, start_peer_download_task,
        detect_player, open_in_player, play_peer_file,
    },
    tasks::{
        add_share, add_shares, cancel_task, clear_activity_log, clear_task, clear_thumb_cache,
        format_bytes_command, generate_pin, get_activity_log, get_firewall_hint, get_lan_urls,
        get_pin, get_qr_for_url, get_server_status, get_share_qr, get_task_progress,
        get_thumb_cache_stats, list_sessions, list_shares, load_config, open_url, path_exists,
        pick_files, pick_folder, regenerate_share_token, remove_share,
        restart_server, revoke_all_sessions, revoke_session, save_config, set_inbox_share,
        set_pin, set_pin_enabled, set_share_enabled, show_in_explorer, start_index_share_task,
        start_prewarm_thumbs_task, start_server, stop_server, update_share,
    },
};

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let state = AppState::new();
            let config = config::load_config_impl(&app.handle());
            config::apply_config_to_state(&state, &config)?;

            if let Ok(dir) = media::thumb_dir(&app.handle()) {
                if let Ok(mut slot) = state.thumb_dir.lock() {
                    *slot = Some(dir);
                }
            }

            // Autostart is off by default -- the first bind to 0.0.0.0 raises
            // the Windows Firewall prompt, and that should happen when the user
            // clicks Start, not silently at launch.
            if config.autostart_server {
                let _ = server::start_server_impl(&app.handle(), &state);
            }

            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // config
            load_config,
            save_config,
            // server
            start_server,
            stop_server,
            restart_server,
            get_server_status,
            // shares
            list_shares,
            add_share,
            add_shares,
            remove_share,
            update_share,
            set_share_enabled,
            regenerate_share_token,
            set_inbox_share,
            // pin
            get_pin,
            set_pin,
            generate_pin,
            set_pin_enabled,
            // network
            get_lan_urls,
            get_share_qr,
            get_qr_for_url,
            get_firewall_hint,
            // activity / sessions
            get_activity_log,
            clear_activity_log,
            list_sessions,
            revoke_session,
            revoke_all_sessions,
            // thumbnails
            start_prewarm_thumbs_task,
            get_thumb_cache_stats,
            clear_thumb_cache,
            // indexing
            start_index_share_task,
            // task machinery
            get_task_progress,
            clear_task,
            cancel_task,
            // devices: this one
            get_device_identity,
            set_device_name,
            // devices: discovery
            list_discovered,
            add_peer_by_address,
            // devices: pairing
            start_pair_task,
            list_incoming_pair_requests,
            accept_pair_request,
            decline_pair_request,
            // devices: the friends list
            list_peers,
            unpair_peer,
            rename_peer,
            set_peer_blocked,
            // network: reading another device's shares
            peer_browse,
            peer_thumb,
            peer_media_url,
            start_peer_download_task,
            // external players
            open_in_player,
            detect_player,
            play_peer_file,
            // os
            pick_folder,
            pick_files,
            show_in_explorer,
            open_url,
            path_exists,
            format_bytes_command,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Stopping here releases the port and the keep-awake lock before
            // the process image goes away. Without it, an in-flight connection
            // leaves the port in TIME_WAIT and an immediate relaunch hits
            // AddrInUse. SO_REUSEADDR is NOT the fix on Windows -- there it
            // means "steal the socket", which is a different and dangerous
            // thing. Graceful shutdown is the fix.
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app.try_state::<AppState>() {
                    let _ = server::stop_server_impl(&state);
                }
            }
        });
}
