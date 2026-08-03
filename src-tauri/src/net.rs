//! LAN address enumeration, URL construction, and QR rendering.
//!
//! Picking the wrong address on a multi-homed machine is the single most common
//! "it doesn't work" report for tools like this, so this module ranks rather
//! than guesses, and the UI shows every candidate.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

use anyhow::{Context, Result};

use crate::models::{LanAddress, LanUrl};

/// Interface-name fragments that are never the address you hand to a phone.
const VIRTUAL_HINTS: &[&str] = &[
    "vethernet",
    "virtualbox",
    "vmware",
    "hyper-v",
    "wsl",
    "docker",
    "loopback",
    "tailscale",
    "zerotier",
    "tap-",
    "tun",
    "bluetooth",
    "npcap",
];

/// The source IP the OS would use to reach the internet.
///
/// No packet is sent -- `connect` on a UDP socket only performs a routing-table
/// lookup and binds the local endpoint. Returns None when there is no default
/// route (offline, or an ad-hoc network).
fn default_route_ip() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect(("1.1.1.1", 80)).ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(v4) if !v4.ip().is_unspecified() => Some(*v4.ip()),
        _ => None,
    }
}

pub(crate) fn lan_addresses() -> Vec<LanAddress> {
    let primary = default_route_ip();
    let mut out: Vec<LanAddress> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|iface| match iface.addr {
            // IPv6 is deliberately dropped: a link-local v6 URL needs a zone
            // index (fe80::1%12) that nobody can type and no QR scanner
            // handles predictably.
            if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() && !v4.ip.is_link_local() => {
                let name_lc = iface.name.to_lowercase();
                Some(LanAddress {
                    ip: v4.ip.to_string(),
                    is_primary: Some(v4.ip) == primary,
                    is_virtual: VIRTUAL_HINTS.iter().any(|h| name_lc.contains(h)),
                    is_private: v4.ip.is_private(),
                    interface: iface.name,
                })
            }
            _ => None,
        })
        .collect();

    // `is_link_local` filters 169.254.x.x (APIPA) -- an address that only
    // appears when DHCP failed, and is guaranteed useless to a phone.
    out.sort_by(|a, b| {
        (!a.is_primary, a.is_virtual, !a.is_private, &a.ip)
            .cmp(&(!b.is_primary, b.is_virtual, !b.is_private, &b.ip))
    });
    out.dedup_by(|a, b| a.ip == b.ip);
    out
}

pub(crate) fn lan_urls(port: u16, path: Option<&str>) -> Vec<LanUrl> {
    let suffix = path.unwrap_or("");
    lan_addresses()
        .into_iter()
        .map(|addr| LanUrl {
            label: addr.interface.clone(),
            url: format!("http://{}:{}{}", addr.ip, port, suffix),
            ip: addr.ip,
            is_primary: addr.is_primary,
            is_virtual: addr.is_virtual,
        })
        .collect()
}

pub(crate) fn hostname() -> Option<String> {
    // No extra dependency: every platform we target exposes this in the
    // environment, and a missing value only costs a default server name.
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

pub(crate) fn qr_svg(url: &str, min_px: u32) -> Result<String> {
    use qrcode::{render::svg, EcLevel, QrCode};
    // EcLevel::M gives 15% recovery. L produces a smaller code, but a phone
    // camera held at an angle across a room needs the redundancy more than it
    // needs 10% fewer modules.
    let code = QrCode::with_error_correction_level(url.as_bytes(), EcLevel::M)
        .context("failed to build QR code")?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(min_px, min_px)
        // Scanners fail without the 4-module quiet zone.
        .quiet_zone(true)
        // ALWAYS dark-on-white, even in a dark theme -- inverted QR codes fail
        // on many Android scanners.
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

/// The exact command a user can paste into an elevated prompt.
///
/// We do NOT shell out to `netsh` ourselves: it requires elevation and fails
/// silently without it, which would leave the user believing the firewall was
/// configured when it was not.
pub(crate) fn firewall_command(port: u16) -> String {
    if cfg!(windows) {
        format!(
            "netsh advfirewall firewall add rule name=\"LAN Share\" dir=in action=allow protocol=TCP localport={port}"
        )
    } else {
        format!("sudo ufw allow {port}/tcp")
    }
}

pub(crate) fn firewall_note() -> String {
    if cfg!(windows) {
        "Windows shows a firewall prompt the first time the server starts, and it defaults to \
         Private networks only. If phones time out, set this Wi-Fi network to Private in Windows \
         Settings -> Network & internet. Note you cannot test this from this PC: connecting to \
         your own LAN IP takes the loopback path and always succeeds."
            .to_string()
    } else {
        "If devices cannot connect, allow inbound TCP on this port in your firewall.".to_string()
    }
}
