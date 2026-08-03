//! LAN presence: a small UDP beacon so two instances find each other without
//! anyone typing an address.
//!
//! Deliberately not mDNS. `mdns-sd` is a heavier dependency and `.local`
//! resolution is unreliable on older Android and on Windows without Bonjour --
//! all to solve a problem we do not have, since we own both ends of this
//! protocol and can define a one-line packet that says exactly what we need.
//!
//! Datagrams go to **both** each interface's directed broadcast address and a
//! link-local multicast group. Ordinary home LANs pass broadcast; some
//! enterprise and mesh setups drop it but pass multicast. Sending both costs
//! one extra `send_to` per interface per five seconds.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use tokio::{net::UdpSocket, sync::watch};

use crate::{
    models::{
        DiscoveredPeer, ServerCtx, ANNOUNCE_EVERY_MS, BEACON_MAGIC, BEACON_MAX_BYTES,
        DEVICE_NAME_MAX, DISCOVERED_CAP, DISCOVERY_MULTICAST,
    },
    utils::now_ms,
};

// ---------------------------------------------------------------------------
// The packet
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Beacon {
    /// `true` for "here I am", `false` for a shutdown goodbye.
    pub(crate) alive: bool,
    pub(crate) device_id: String,
    pub(crate) port: u16,
    pub(crate) platform: String,
    pub(crate) name: String,
}

/// Field separator. Names are base64url-encoded precisely so a space or a
/// newline in a device name cannot add or shift fields.
const SEP: char = ' ';

impl Beacon {
    /// `LANSHARE/1 <hi|bye> <device_id> <port> <platform> <name-b64>`
    ///
    /// Broadcast reaches every machine on the subnet, so this may only carry
    /// what a stranger is allowed to know. No share names, no PIN, no paths.
    pub(crate) fn encode(&self) -> Vec<u8> {
        // Clamped here as well as at config time. `decode` refuses anything
        // over BEACON_MAX_BYTES, so emitting an oversized packet would make
        // this device silently undiscoverable -- a failure with no error
        // anywhere, on either machine. 48 chars is at most 192 UTF-8 bytes,
        // which base64s to 256 and leaves ample room for the fixed fields.
        let name: String = self.name.chars().take(DEVICE_NAME_MAX).collect();
        format!(
            "{} {} {} {} {} {}",
            BEACON_MAGIC,
            if self.alive { "hi" } else { "bye" },
            self.device_id,
            self.port,
            self.platform,
            b64_encode(name.as_bytes()),
        )
        .into_bytes()
    }

    /// Every field is validated. This parses data from an unauthenticated
    /// stranger on the network, so it returns `None` rather than partial
    /// results and never allocates proportional to attacker input.
    pub(crate) fn decode(raw: &[u8]) -> Option<Self> {
        if raw.is_empty() || raw.len() > BEACON_MAX_BYTES {
            return None;
        }
        let text = std::str::from_utf8(raw).ok()?;
        let mut parts = text.split(SEP);

        if parts.next()? != BEACON_MAGIC {
            return None;
        }
        let alive = match parts.next()? {
            "hi" => true,
            "bye" => false,
            _ => return None,
        };

        let device_id = parts.next()?;
        // The id is echoed into the UI and used as a map key; keep it to the
        // shape our own generator produces.
        if device_id.is_empty()
            || device_id.len() > 64
            || !device_id.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return None;
        }

        let port: u16 = parts.next()?.parse().ok()?;
        if port == 0 {
            return None;
        }

        let platform = parts.next()?;
        if platform.is_empty()
            || platform.len() > 16
            || !platform.chars().all(|c| c.is_ascii_lowercase() || c == '-')
        {
            return None;
        }

        let name_raw = parts.next()?;
        // Exactly six fields. A trailing extra means this is not our packet.
        if parts.next().is_some() {
            return None;
        }
        let name_bytes = b64_decode(name_raw)?;
        let name = String::from_utf8(name_bytes).ok()?;
        // Same cleaning the config applies: strip controls and bidi overrides,
        // then clamp. A remote device does not get to reorder our UI.
        let name = crate::config::clean_device_name(&name);

        Some(Beacon {
            alive,
            device_id: device_id.to_string(),
            port,
            platform: platform.to_string(),
            name: name.chars().take(DEVICE_NAME_MAX).collect(),
        })
    }
}

// --- base64url, just enough for a device name ------------------------------

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64_encode(input: &[u8]) -> String {
    // Unpadded: '=' would be another character to validate for no benefit.
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        let take = chunk.len() + 1;
        for i in 0..take {
            out.push(B64[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
        }
    }
    out
}

fn b64_decode(input: &str) -> Option<Vec<u8>> {
    if input.len() > BEACON_MAX_BYTES {
        return None;
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for chunk in input.as_bytes().chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut n: u32 = 0;
        for (i, c) in chunk.iter().enumerate() {
            let v = B64.iter().position(|b| b == c)? as u32;
            n |= v << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            out.push(((n >> (16 - 8 * i)) & 0xFF) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Sockets
// ---------------------------------------------------------------------------

/// Bind the shared send/receive socket.
///
/// Returns a **std** socket, not a tokio one: `UdpSocket::from_std` panics
/// without a running reactor, and this is deliberately called from the Tauri
/// command thread so that a bind failure is a plain `Err` and so that the
/// Windows firewall prompt lands beside the TCP one. `adopt` completes the
/// handover once we are inside the runtime -- the same split the TCP listener
/// already uses.
///
/// `SO_REUSEADDR` before bind is required: without it a second instance on one
/// machine fails to bind, and one machine running two instances is exactly the
/// configuration you develop and test against.
pub(crate) fn bind_receiver(port: u16) -> Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("failed to create the discovery socket")?;
    socket
        .set_reuse_address(true)
        .context("failed to set SO_REUSEADDR on the discovery socket")?;
    socket
        .set_broadcast(true)
        .context("failed to enable broadcast on the discovery socket")?;
    socket
        .bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())
        .with_context(|| format!("failed to bind UDP port {port} for discovery"))?;
    socket
        .set_nonblocking(true)
        .context("failed to set the discovery socket non-blocking")?;

    let std_socket: std::net::UdpSocket = socket.into();

    // Join the group on every IPv4 interface. INADDR_ANY alone picks one
    // interface by routing table, which on a multi-homed machine is routinely
    // the wrong one (Hyper-V or WSL rather than the Wi-Fi).
    let group = Ipv4Addr::from(DISCOVERY_MULTICAST);
    let mut joined = 0usize;
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            if v4.ip.is_loopback() {
                continue;
            }
            if std_socket.join_multicast_v4(&group, &v4.ip).is_ok() {
                joined += 1;
            }
        }
    }
    // Not fatal: broadcast alone still works, and a machine with no usable
    // interface has nothing to discover anyway.
    let _ = joined;

    // Loopback ON so we hear our OWN beacon. That is not vanity -- it is the
    // only cheap signal that the inbound path actually works, which is what
    // `health_from` uses to tell a firewall block from an empty network.
    let _ = std_socket.set_multicast_loop_v4(true);

    Ok(std_socket)
}

/// Hand the bound socket to tokio. Must run inside the runtime.
pub(crate) fn adopt(socket: std::net::UdpSocket) -> Result<UdpSocket> {
    UdpSocket::from_std(socket).context("failed to register the discovery socket with tokio")
}

/// Every address a beacon should be sent to: each interface's directed
/// broadcast, plus the multicast group.
fn announce_targets(port: u16) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();

    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            if v4.ip.is_loopback() || v4.ip.is_link_local() {
                continue;
            }
            let bcast = directed_broadcast(v4.ip, v4.netmask);
            let addr = SocketAddr::V4(SocketAddrV4::new(bcast, port));
            if !out.contains(&addr) {
                out.push(addr);
            }
        }
    }

    out.push(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::from(DISCOVERY_MULTICAST),
        port,
    )));
    out
}

/// `ip | !mask`. A /24 host at 192.168.1.77 gives 192.168.1.255.
///
/// A zero mask would yield the global broadcast address, which many stacks
/// refuse to route; fall back to it explicitly so the intent is visible.
pub(crate) fn directed_broadcast(ip: Ipv4Addr, mask: Ipv4Addr) -> Ipv4Addr {
    let ip = u32::from(ip);
    let mask = u32::from(mask);
    if mask == 0 {
        return Ipv4Addr::BROADCAST;
    }
    Ipv4Addr::from(ip | !mask)
}

// ---------------------------------------------------------------------------
// Loops
// ---------------------------------------------------------------------------

fn beacon_for(ctx: &ServerCtx, alive: bool) -> Beacon {
    let (name, port) = ctx
        .settings
        .read()
        .map(|s| (s.device_name.clone(), s.port))
        .unwrap_or_else(|_| (String::new(), 0));
    Beacon {
        alive,
        device_id: ctx.device_id.as_str().to_string(),
        port,
        platform: crate::models::platform_name(),
        name,
    }
}

/// What the announce loop owes the network on this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BeaconAction {
    /// "Here I am", repeated every tick while visible.
    Announce,
    /// "I'm going" -- sent once, on the way from visible to hidden.
    Goodbye,
    Nothing,
}

/// The visibility rule, pulled out of the loop so it can be tested without a
/// socket. `announcing` is what the network currently believes.
///
/// A goodbye is owed exactly once per visible period: repeating it every tick
/// while hidden would be pointless traffic, and skipping it altogether leaves
/// this device on everyone else's screen until the 20-second offline threshold
/// expires -- which is what makes a visibility switch feel broken.
pub(crate) fn beacon_action(visible: bool, port: u16, announcing: bool) -> BeaconAction {
    // No port, no packet -- not even a goodbye, since there is nowhere to
    // address it. Changing the port needs a rebind anyway.
    if port == 0 {
        return BeaconAction::Nothing;
    }
    match (visible, announcing) {
        (true, _) => BeaconAction::Announce,
        (false, true) => BeaconAction::Goodbye,
        (false, false) => BeaconAction::Nothing,
    }
}

/// One beacon to every broadcast address and the multicast group. Best effort
/// throughout: a single unreachable interface must not stop the others.
async fn send_beacon(ctx: &ServerCtx, socket: &UdpSocket, port: u16, alive: bool) {
    if port == 0 {
        return;
    }
    let packet = beacon_for(ctx, alive).encode();
    for target in announce_targets(port) {
        let _ = socket.send_to(&packet, target).await;
    }
}

/// Announce while visible; say goodbye the moment we stop being.
///
/// The visibility switch lives here rather than in the socket's lifetime: the
/// socket stays bound either way, because being invisible has never meant
/// being blind, and rebinding it would need a server restart.
pub(crate) async fn announce_loop(
    ctx: ServerCtx,
    socket: Arc<UdpSocket>,
    mut shutdown: watch::Receiver<bool>,
) {
    ctx.discovery_started_ms.store(now_ms(), Ordering::Relaxed);
    let mut tick = tokio::time::interval(Duration::from_millis(ANNOUNCE_EVERY_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval`'s first tick completes immediately; consume it here so the
    // announce below is not doubled on the first pass through the loop.
    tick.tick().await;

    // What the rest of the network currently believes about us. Exactly one
    // goodbye is sent per visible period -- none if we never announced, and
    // none repeated while already hidden.
    let mut announcing = false;

    loop {
        // Read live: renaming the device or flipping "visible" takes effect on
        // the next tick, and at once when the toggle wakes us.
        let (visible, port) = ctx
            .settings
            .read()
            .map(|s| (s.discoverable && s.peering_enabled, s.discovery_port))
            .unwrap_or((false, 0));

        match beacon_action(visible, port, announcing) {
            BeaconAction::Announce => {
                send_beacon(&ctx, &socket, port, true).await;
                announcing = true;
            }
            // Switched to invisible. Announce the departure instead of merely
            // falling silent: silence takes the full 20-second offline
            // threshold to register, which reads as a button that did nothing.
            BeaconAction::Goodbye => {
                send_beacon(&ctx, &socket, port, false).await;
                announcing = false;
            }
            BeaconAction::Nothing => {}
        }

        tokio::select! {
            _ = tick.tick() => {}
            // `set_discoverable` pokes this, so the switch acts now.
            _ = ctx.discovery_wake.notified() => {}
            _ = shutdown.wait_for(|stop| *stop) => break,
        }
    }

    // Goodbye, best effort. It turns "offline in 20 seconds" into "offline
    // now" on every other device, which is the difference between the list
    // feeling live and feeling stale.
    if announcing {
        let port = ctx.settings.read().map(|s| s.discovery_port).unwrap_or(0);
        send_beacon(&ctx, &socket, port, false).await;
    }
}

pub(crate) async fn receive_loop(
    ctx: ServerCtx,
    socket: Arc<UdpSocket>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut buf = vec![0u8; BEACON_MAX_BYTES];
    loop {
        tokio::select! {
            received = socket.recv_from(&mut buf) => {
                let Ok((len, from)) = received else { continue };
                let Some(beacon) = Beacon::decode(&buf[..len]) else { continue };
                apply_beacon(&ctx, &beacon, from.ip());
            }
            _ = shutdown.wait_for(|stop| *stop) => break,
        }
    }
}

/// Fold one received beacon into the discovered table.
///
/// Split out from the loop so it is testable without a socket.
pub(crate) fn apply_beacon(ctx: &ServerCtx, beacon: &Beacon, from: IpAddr) {
    // Our own packet, looped back. Filtered by device_id and NOT by source IP:
    // filtering on IP would also hide a second instance running on this same
    // machine, which is the configuration we develop against.
    if beacon.device_id == *ctx.device_id {
        ctx.discovery_self_seen_ms.store(now_ms(), Ordering::Relaxed);
        return;
    }

    let Ok(mut table) = ctx.discovered.lock() else {
        return;
    };
    let now = now_ms();
    let address = from.to_string();

    if !beacon.alive {
        // Goodbye: keep the row so the UI can show "offline" rather than
        // having the device silently vanish mid-glance, but back-date it past
        // the offline threshold immediately.
        if let Some(entry) = table.get_mut(&beacon.device_id) {
            entry.last_seen_ms = now.saturating_sub(crate::models::PEER_OFFLINE_AFTER_MS + 1);
        }
        return;
    }

    match table.get_mut(&beacon.device_id) {
        Some(entry) => {
            entry.name = beacon.name.clone();
            entry.platform = beacon.platform.clone();
            entry.port = beacon.port;
            entry.last_seen_ms = now;
            // A machine with Wi-Fi and Ethernet on one subnet announces from
            // both; remember every address, newest first, so a send can fall
            // back if one path stops working.
            entry.addresses.retain(|a| a != &address);
            entry.addresses.insert(0, address);
            entry.addresses.truncate(4);
        }
        None => {
            // A forged-id flood must not grow this without bound. Evict the
            // least recently seen rather than refusing the new one, so a flood
            // cannot lock out a genuine device either.
            if table.len() >= DISCOVERED_CAP {
                if let Some(oldest) = table
                    .values()
                    .min_by_key(|p| p.last_seen_ms)
                    .map(|p| p.device_id.clone())
                {
                    table.remove(&oldest);
                }
            }
            table.insert(
                beacon.device_id.clone(),
                DiscoveredPeer {
                    device_id: beacon.device_id.clone(),
                    name: beacon.name.clone(),
                    platform: beacon.platform.clone(),
                    addresses: vec![address],
                    port: beacon.port,
                    last_seen_ms: now,
                    manual: false,
                },
            );
        }
    }
}

/// Drop devices we have not heard from in a long time. Manually added entries
/// survive: the user typed those in, so forgetting them is data loss, not
/// tidying.
pub(crate) fn sweep(ctx: &ServerCtx) {
    let Ok(mut table) = ctx.discovered.lock() else {
        return;
    };
    let now = now_ms();
    table.retain(|_, peer| {
        peer.manual || now.saturating_sub(peer.last_seen_ms) <= crate::models::PEER_EVICT_AFTER_MS
    });
}

/// Best-effort read on whether the inbound path works.
///
/// With multicast loopback on, our own beacon returns every tick. If we have
/// been announcing for a while and have not seen ourselves for several
/// intervals, inbound datagrams are probably being dropped. This is a
/// **heuristic, not a proof** -- whether a locally looped datagram traverses
/// the Windows filtering platform varies with the profile and with third-party
/// security software -- so callers must phrase it as "may be blocked", never
/// as a fact.
///
/// Pure and argument-taking, because both a `ServerCtx` (the HTTP side) and an
/// `AppState` (the Tauri command side) need it and they hold the same Arcs
/// under different names. Two copies of a heuristic drift.
pub(crate) fn health_from(started_ms: u64, self_seen_ms: u64, nothing_discovered: bool) -> &'static str {
    if started_ms == 0 {
        return "ok";
    }
    let now = now_ms();
    if now.saturating_sub(started_ms) < 30_000 {
        // Too early to conclude anything.
        return "ok";
    }
    if now.saturating_sub(self_seen_ms) > 3 * ANNOUNCE_EVERY_MS {
        return "inbound_likely_blocked";
    }
    if nothing_discovered {
        // Our own inbound path works, so this is not a firewall problem. Most
        // often it is AP client isolation, which no firewall change fixes.
        return "nothing_heard";
    }
    "ok"
}

/// Parse a user-typed `host` or `host:port` for the manual-add fallback.
pub(crate) fn parse_manual_address(input: &str, default_port: u16) -> Result<(IpAddr, u16)> {
    let text = input.trim();
    if text.is_empty() {
        return Err(anyhow!("enter an address"));
    }
    // Reject a URL rather than silently ignoring the scheme -- pasting one is
    // the obvious mistake, and a clear message is better than a wrong parse.
    if text.contains("://") {
        return Err(anyhow!("enter just the address, without http://"));
    }
    if let Ok(addr) = text.parse::<SocketAddr>() {
        if addr.port() == 0 {
            return Err(anyhow!("port must not be zero"));
        }
        return Ok((addr.ip(), addr.port()));
    }
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Ok((ip, default_port));
    }
    Err(anyhow!("{text:?} is not a valid IP address"))
}
