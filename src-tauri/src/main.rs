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
        accept_pair_request, add_peer_by_address, decline_pair_request, list_discovered,
        list_incoming_pair_requests, list_peers, rename_peer, set_peer_blocked, start_pair_task,
        unpair_peer,
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

            #[cfg(windows)]
            for window in app.webview_windows().values() {
                use_native_size_icons(window);
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

// ---------------------------------------------------------------------------
// Window icon (Windows)
// ---------------------------------------------------------------------------

/// Hand the shell the icon at the size it is about to draw, instead of one
/// bitmap for every size.
///
/// Tauri's `default_window_icon` is produced by `tauri-codegen`, which opens
/// `icon.ico` and takes `entries()[0]` -- the FIRST entry in the directory, not
/// the best one, not the biggest. Ours is written smallest-first, so that is the
/// 16px image, and tao then installs it as both ICON_SMALL and ICON_BIG. Every
/// surface that wants more than 16 device pixels -- the taskbar button, Alt-Tab,
/// Task View -- gets it magnified.
///
/// That is both halves of the bug report. It is blurry because 16 -> 24 is an
/// upscale, and it does not match the icon Explorer draws because the 16px tier
/// deliberately carries no nodes and no wires (see `tools/make-icons.mjs`): what
/// was being magnified was the stripped-down glyph, not the mark.
///
/// The ladder is already in the executable -- `tauri-build` links `icon.ico` as
/// resource 32512 -- so the fix is to ask for it BY SIZE. Given an explicit
/// cx/cy, `LoadImageW` runs the shell's own best-match over the directory, and
/// the two metrics it is asked for land on entries that were drawn at exactly
/// that size:
///
///   SM_CXSMICON   16 / 20 / 24 / 32     at 100 / 125 / 150 / 200% DPI
///   SM_CXICON     32 / 40 / 48 / 64     at the same four
///
/// which is why 20 and 40 are in the ladder at all.
///
/// `GetSystemMetricsForDpi` rather than `GetSystemMetrics`: the process is
/// per-monitor DPI aware, so the un-suffixed call answers for the primary
/// monitor and would pick the wrong entry everywhere else. It is read once, at
/// startup, from the window's own monitor -- not refreshed on a DPI change,
/// because the surface that matters most is the taskbar, and the taskbar scales
/// to ITS monitor, which is not necessarily the one the window is on. There is
/// no single answer to re-read.
///
/// The two icons are deliberately never destroyed: they stay installed on the
/// window for the life of the process, and `DestroyIcon` on the handle the
/// window is still using is a use-after-free. They are two small bitmaps, and
/// the process exiting is what frees them.
///
/// Every step is best-effort. The worst case is the icon Tauri already set,
/// which is what shipped, so there is nothing to report and nothing to fail.
#[cfg(windows)]
fn use_native_size_icons(window: &tauri::WebviewWindow) {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HINSTANCE, LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
    use windows::Win32::UI::WindowsAndMessaging::{
        LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, SM_CXICON,
        SM_CXSMICON, SM_CYICON, SM_CYSMICON, WM_SETICON,
    };

    let Ok(hwnd) = window.hwnd() else { return };
    let module = unsafe { GetModuleHandleW(None) };
    let Ok(module) = module else { return };
    let instance = HINSTANCE(module.0);

    // Zero means the handle was not a window; 96 is the 100% baseline.
    let dpi = match unsafe { GetDpiForWindow(hwnd) } {
        0 => 96,
        dpi => dpi,
    };

    // Small is the title bar and the window list. Big is Alt-Tab, Task View and
    // the taskbar button.
    for (slot, cx_metric, cy_metric) in [
        (ICON_SMALL, SM_CXSMICON, SM_CYSMICON),
        (ICON_BIG, SM_CXICON, SM_CYICON),
    ] {
        let cx = unsafe { GetSystemMetricsForDpi(cx_metric, dpi) };
        let cy = unsafe { GetSystemMetricsForDpi(cy_metric, dpi) };

        // 32512 is IDI_APPLICATION, which is the id tauri-build links the .ico
        // under -- the same convention as LoadIcon(hInstance, IDI_APPLICATION).
        // It is an ordinal, not a pointer: MAKEINTRESOURCE is a cast.
        let icon = unsafe {
            LoadImageW(
                Some(instance),
                PCWSTR(32512 as *const u16),
                IMAGE_ICON,
                cx,
                cy,
                LR_DEFAULTCOLOR,
            )
        };

        if let Ok(icon) = icon {
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_SETICON,
                    Some(WPARAM(slot as usize)),
                    Some(LPARAM(icon.0 as isize)),
                );
            }
        }
    }
}
