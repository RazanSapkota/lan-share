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
mod transfer;
#[cfg(test)]
mod tests;
mod utils;
mod zipstream;

use tauri::Manager;

use crate::{
    models::AppState,
    tasks::{
        accept_offer, accept_pair_request, add_peer_by_address, cancel_transfer, decline_offer,
        decline_pair_request, get_device_identity, list_discovered, list_incoming_offers,
        list_incoming_pair_requests, list_peers, list_transfers, peer_browse, pick_receive_folder,
        rename_peer, set_device_name, set_discoverable, set_peer_auto_accept, set_peer_blocked,
        start_pair_task, start_send_files_task, unpair_peer,
        create_handoff, revoke_handoff, list_handoffs,
        detect_player, open_in_player, play_peer_file,
    },
    tasks::{
        add_share, add_shares, cancel_task, clear_activity_log, clear_task, clear_thumb_cache,
        format_bytes_command, generate_pin, get_activity_log, get_firewall_hint, get_lan_urls,
        get_pin, get_qr_for_url, get_server_status, get_share_qr, get_task_progress,
        get_thumb_cache_stats, list_sessions, list_shares, load_config, open_url, path_exists,
        pick_files, pick_folder, preview_share, regenerate_share_token, remove_share,
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

            // Resolve the receive folder up front so the Devices page can show
            // it before the server has ever run, and clear any `.part` files a
            // crash mid-transfer left behind -- the one case nothing else
            // cleans up, since the normal paths delete on every failure.
            if let Ok(dir) = peers::resolve_receive_dir(&app.handle(), &state) {
                transfer::sweep_parts(&dir, 0);
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
            preview_share,
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
            // peers: this device
            get_device_identity,
            set_device_name,
            set_discoverable,
            pick_receive_folder,
            // peers: discovery
            list_discovered,
            add_peer_by_address,
            // peers: pairing
            start_pair_task,
            list_incoming_pair_requests,
            accept_pair_request,
            decline_pair_request,
            // peers: the friends list
            list_peers,
            unpair_peer,
            rename_peer,
            set_peer_blocked,
            set_peer_auto_accept,
            // peers: transfers
            start_send_files_task,
            list_transfers,
            cancel_transfer,
            list_incoming_offers,
            accept_offer,
            decline_offer,
            peer_browse,
            // external players
            open_in_player,
            detect_player,
            play_peer_file,
            // send to phone
            create_handoff,
            revoke_handoff,
            list_handoffs,
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
