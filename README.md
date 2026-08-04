# LAN Share

Share folders and media over your local network. You pick folders in a desktop
app; anyone on the same Wi-Fi opens a link in their browser and browses,
streams or downloads the files. Nothing to install on their side.

Built with the same stack and conventions as the sibling `src-2` app: Rust +
Tauri v2, vanilla HTML/CSS/JS, **no bundler and no npm dependencies**.

![Desktop dashboard](docs/screenshots/desktop-dashboard.png)

## What it does

- **Find other devices on the network automatically**, and connect to them from
  one list the way you would pick a Wi-Fi network — except you can be connected
  to several at once. Connect once per device; the list survives restarts.
- **Browse what a connected device shares**, on a page of its own — thumbnails, a
  full-size preview, video that seeks, and downloads either as separate files or
  as one `.zip`, your choice.
- **One switch.** Starting the server is the whole decision: it shares your
  folders and makes you findable, because those were never two questions.
- **Share folders or individual files.** Each share gets its own secret link, so
  you can hand out one folder without exposing the rest.
- **A 6-digit PIN** gates the whole server (toggleable). Wrong-PIN attempts are
  rate-limited per device.
- **Media gallery on the receiving end** — image thumbnails, inline video and
  audio playback with real seeking, an image lightbox, and a list-view toggle.
- **Anything a browser won't play — `.mkv`, `.avi`, HEVC, DTS — opens in VLC**
  or whatever player the device already has, streaming from the host with no
  download and no conversion.
- **Per-file downloads**, plus an optional streamed `.zip` of a whole folder.
- **Optional upload inbox.** Off by default; when on, exactly one folder accepts
  uploads and nothing is ever overwritten.
- **Live activity log** showing who fetched what, how many bytes actually made it,
  and whether the transfer finished or died mid-stream.
- **QR code** so a phone joins by pointing its camera at the screen.

### Device to device, in short

Two computers running LAN Share on one network see each other within about five
seconds. Click **Connect** on one; both then show the same six-digit code and you
tap **Accept** on the other. Nothing to type — and the matching code is what
stops a stranger on the same Wi-Fi from impersonating your laptop.

After that each can browse the other's shares without a PIN, from the
**Devices** page. **Connections** is where you find machines and connect to or
disconnect from them; **Shares** is what you hand out; **Devices** is what you
can reach.

The three buttons map onto Wi-Fi's, because the states really are the same ones:
**Connect** is the six-digit handshake, **Disconnect** stops traffic both ways
but keeps the connection so reconnecting needs no code, and **Forget** throws it
away so both sides start over. Underneath they are still pair, block and unpair.

**Nothing pushes.** A device cannot send you a file, only offer one for you to
come and take. That is a smaller protocol and a much smaller question — there is
no prompt to accept, no folder to nominate in advance, and no way for a connected
machine to put bytes on your disk while you are not looking.

**Phones can't be peers** — there is no desktop binary for them. They keep the
browser flow: open the address, type the PIN, browse and download.

## Running it

```sh
npm run tauri:dev      # cargo run --manifest-path src-tauri/Cargo.toml
npm run tauri:build    # release build
npm test               # cargo test  (158 tests)
```

There is no frontend build step — `ui/` is served straight from disk by Tauri,
and `src-tauri/web/` is compiled into the binary with `include_str!`.

### First run

1. **Shares** → *Add folder*.
2. **Connections** → flip the switch on.
3. Windows raises a firewall prompt. Tick **Private**, and also **Public** if
   you will use this on a café or hotel network.
4. Scan the QR code with a phone, or type the address in, then enter the PIN.

If a phone times out at step 4, the network is almost certainly classified
*Public* while the firewall rule is Private-only — Windows Settings → Network &
internet → Wi-Fi → *your network* → Private. You cannot reproduce this from the
host machine: connecting to your own LAN IP takes the loopback path and always
succeeds.

## Layout

```
ui/                        desktop control panel  (frontendDist, Tauri webview)
tools/icon-source.png      the artwork every icon is cut from
tools/make-icons.mjs       that png -> png/ico/icns        (run by hand)
src-tauri/
  icons/                   window, taskbar, favicon and manifest icons
  web/                     receiver web app       (include_str!'d into the exe)
  src/
    main.rs                Builder, command registry, RunEvent::Exit hook
    models.rs              AppState, AppConfig, every wire struct
    config.rs              JSON config load/save + derived-state rebuild
    server.rs              bind, runtime thread, router, graceful shutdown
    routes.rs              every axum handler
    auth.rs                PIN, sessions, share + pair tokens, the extractor
    shares.rs              path containment + directory listing
    media.rs               file-kind classification, thumbnails
    net.rs                 LAN address ranking, QR
    activity.rs            in-memory log + byte-counting response body
    assets.rs              embedded receiver UI
    zipstream.rs           forward-only ZIP writer
    discovery.rs           UDP beacon: encode/decode, announce + receive loops
    peers.rs               pairing state machine + the /api/peer/* handlers
    peerclient.rs          the outbound half: pairing, browsing, pulling
    tasks.rs               every #[tauri::command]
    utils.rs, tests.rs
```

The desktop panel divides by *responsibility*: **Connections** is both
directions of reach — the switch, the link, the QR and the PIN that let phones
get to you, and below them the list of other machines you can connect to.
**Shares** is what you hand out. **Devices** is what those machines hand back,
and everything you do with it. They were one page once, and it did all of it
badly.

Connecting used to be a page of its own called Network, which named the wire
rather than the job. It moved onto the Connections page because both halves
answer the same question — who can reach whom — and because its own list had to
reach across and read the server's state twice to say anything useful.

Note that the section ids no longer match their labels: `#dashboard-view` is
the Connections page and `#network-view` is the Devices page. Renaming them
would touch the `PAGES` → `#{key}-view` routing, and `switchPage` falls back to
`"dashboard"`, for no gain a user could see.

The **two-frontend split** is the central structural decision: `ui/` is what the
app window shows, `src-tauri/web/` is what phones see. The receiver UI uses no
external fonts, CDNs or libraries, because the device on the other end may have
no internet access at all — only a route to this host.

## Notes on a few decisions

**The icons are derived, not committed by hand.** `tools/icon-source.png` is the
artwork; `tools/make-icons.mjs` cuts every other size from it and writes each
PNG, the `.ico` and the `.icns` using nothing but Node's `zlib` — PNG, ICO and
ICNS are all simple enough containers to emit by hand, and that beats adding an
image toolchain to a project whose whole boast is not having one. Delete
`src-tauri/icons/` and one command puts it back.

The source is an 800px squircle that fills its canvas, so the crop-to-alpha-
bounds step is a no-op on it — it is there because the artwork before this one
sat 839px inside a 1024 canvas, and that padding made every icon render a size
smaller than its neighbours in the taskbar. The box filter still earns its keep
at the corners: it weights colour by alpha rather than averaging samples, and
the transparent pixels outside the squircle are pure black, so a straight
average would draw a dark fringe around all four of them.

**There is one mark, and it is that file.** The window header, the PIN screen
and the tab icon all show it: `ui/icon.png` for the desktop panel (which can
only read files under `frontendDist`), `/assets/icon-192.png` for the guest
page, where one fetch serves both the tab and the PIN screen. They used to be
inline SVG — a hand-drawn folder with two arcs, in `currentColor` so it followed
the theme — which was quietly a different logo from the one on the taskbar.

The `.ico` carries ten sizes, not the usual handful, because a missing size is
not a missing icon — the shell scales a neighbour instead, and that upscale is
what "blurry icon" actually is. 16/20/24/32 for lists and the title bar, 40 at
125% DPI, 48/64 for the taskbar and Alt-Tab, 96/128/256 for Explorer. Entries
below 64px are uncompressed DIBs and the rest are PNG, which is what keeps the
file at 54 KB: the same ten sizes as DIBs throughout would be 409 KB, because a
DIB costs `w × h × 4` whatever is drawn on it — the 256 alone is 264 KB.

**`build.rs` watches the icons, and has to.** `tauri_build` compiles
`icons/icon.ico` into a Windows resource, but the only path it registers with
cargo is `tauri.conf.json`. Replace the icon and nothing re-runs: cargo relinks
against a stale `resource.lib` and the binary keeps the icon you just replaced,
silently. The two `rerun-if-changed` lines in `build.rs` are the fix, and the
symptom they cure — a rebuilt exe still showing the old icon — is worth
recognising, because nothing about it looks like a build problem.

**Storage is a single JSON config file.** No database. Directory listings are
read live from disk; the activity log lives in memory and clears on quit;
thumbnails go in an on-disk cache keyed by `path + mtime + size`.

**`.mkv` is handed to a player, not converted.** No browser decodes Matroska
and none can be taught to — there is no codec to install, and VLC is a player
rather than something a page can borrow from. Transcoding on the host was the
alternative: it needs ffmpeg installed or bundled, burns a core per viewer, and
still cannot seek properly without building HLS on top. Meanwhile the phone in
your hand already has software that plays the file perfectly. So **Open in
player** mints a link and hands the stream over — Android gets an intent URL
and a real app chooser, iOS gets VLC's callback scheme. Desktop browsers have
neither: no browser exposes an app chooser, so there is nothing to call. They
get a panel with the stream link, a Copy button and the *Open Network Stream*
keystroke, plus the one-line `.m3u` for anyone whose player is already
associated with playlists. Seeking works
because it is the same `Range` handling the browser path uses. This covers
`.avi`, `.wmv`, `.ts`, HEVC and AC3 for free, and it is offered next to files
that *do* play inline, because a phone often handles a big one better in a real
player.

**A play link is not a session.** A player cannot hold our cookie, so the
credential has to ride in the URL — the exact thing the session design refuses,
one paragraph down. What makes it acceptable is how little it carries: one
file, six hours, accepted only on `/play/*` and `/playlist/*` and nowhere else,
capped at 64 live, re-checked against the share on every request so switching a
share off cuts the stream, and gone when the server stops. Pasted into a group
chat it leaks the film someone was already watching, not the run of the house.
The URL ends in the real filename, dot and all — a player picks its demuxer
from the extension it can see, so `movie%2Emkv` would be a file of unknown type.

**Auth rides on a cookie, not a header.** `<video src>` and `<img src>` cannot
send `Authorization`, a Service Worker needs a secure context that plain-HTTP
LAN addresses do not have, and a query-string token would leak the whole session
the moment someone pastes a video link into a group chat. The cookie is
`HttpOnly; SameSite=Lax`, with an `X-LanShare` header as a second CSRF lock.

**Path containment rejects rather than sanitizes.** `shares.rs` refuses `..`,
rooted and drive-relative forms, NTFS alternate data streams, reserved device
names, trailing dots and spaces, and control characters — then canonicalizes and
re-checks containment as a backstop against symlinks and junctions. Roughly a
third of the test suite covers this one file.

**The ZIP writer is hand-rolled.** The `zip` crate seeks backwards to patch each
local header once the size is known, which a socket cannot do — it can only
write to a file. `zipstream.rs` writes the same format forward-only using data
descriptors, so a large folder starts downloading immediately instead of being
staged to disk first. Its output is tested by reading it back with the `zip`
crate.

**Nothing autostarts.** The first bind to `0.0.0.0` raises the Windows firewall
prompt, and that should happen when you click Start — a moment you have context
for — not silently at launch. Discovery is UDP, and Windows prompts **per
protocol**, so expect a second dialog; the socket is bound next to the TCP one
precisely so both land in the same gesture. Denying it leaves file sharing
working and falls back to **Add by address**.

**Every control explains itself on hover.** Anything with a `data-tip`
attribute gets a tooltip, delegated from `document` and drawn into one element
parked on `<body>`. Not `title`: the native tooltip takes about a second, comes
in the OS font, and never appears for someone tabbing with a keyboard. Not a
CSS `::after` either — half these buttons sit inside the shares table and the
dialogs, which scroll and therefore clip, so a pseudo-element tip would be cut
off exactly where it is needed. The delegation is what makes a tip on a
rendered row a one-attribute change. The receiver UI keeps plain `title`
attributes instead: phones have no hover, and it ships no JS it does not need.

**Hidden is a beacon that stops, not a socket that closes.** The UDP socket is
bound for as long as the server runs, so going hidden costs nothing and coming
back needs no restart — an earlier version tied the socket to the flag, which
made "hidden" mean deaf as well as silent and tore down every in-flight
download each time someone switched back on. Two details make the switch feel
instant: flipping it pokes the announce loop instead of waiting up to five
seconds for its next tick, and switching off sends a **goodbye beacon** rather
than merely falling silent — silence takes the full twenty-second offline
threshold to register on the other machine, which reads as a button that did
nothing.

**Pairing commits before it compares.** The initiator hashes its nonce and sends
only the hash; the responder replies with its own nonce; the code is derived
from both, and only then does the initiator reveal. Without that commitment an
attacker running both halves of the exchange could pick its nonces *after*
seeing yours and grind the two codes into agreement — which is the entire attack
numeric comparison exists to stop. It is what Bluetooth SSP does, for the same
reason.

**Devices pull; nothing pushes.** Two devices used to be able to send files at
each other, which needed an offer protocol, an accept prompt, a nominated
receive folder, a `.part` write path and a transfer table — around 1,500 lines
whose entire job was making it safe for someone else to write to your disk.
Browsing replaces all of it: the Devices page reads over the same routes a
browser uses, and a download is this machine deciding to fetch something. The
offer routes are gone from the router, and a test asserts they stay gone.

**Peers reuse the browser's routes, not a parallel API.** A paired device gets a
real `SessionScope::Peer`, so `/api/list` and `/files/…` serve it unchanged —
which means the path-traversal test table can be replayed with a bearer token
and prove the two paths are the same code. The one sharp edge: the extractor's
"PIN disabled" branch admits anything, so the peer check runs *before* it and
returns an error for a blocked device rather than falling through. There is a
named regression test for exactly that.

**Two tokens per pairing, not one shared secret.** `in_token` is what they
present to you (you issued it); `out_token` is what you present to them. Either
direction can be revoked alone, and a leaked token cannot be replayed back at
its issuer.

## Screenshots

| Receiver — PIN | Receiver — gallery | Receiver — list |
|---|---|---|
| ![](docs/screenshots/receiver-pin.png) | ![](docs/screenshots/receiver-gallery.png) | ![](docs/screenshots/receiver-list.png) |

| Desktop — shares | Desktop — activity |
|---|---|
| ![](docs/screenshots/desktop-shares.png) | ![](docs/screenshots/desktop-activity.png) |

| Desktop — connections | Connection code |
|---|---|
| ![](docs/screenshots/desktop-devices.png) | ![](docs/screenshots/pairing-code.png) |

The Activity and device shots predate the split into Connections / Shares /
Devices, and there is no Devices shot yet.

## License

[MIT](LICENSE) — do what you like with it, keep the copyright notice, and
expect no warranty. The Rust crates it builds on keep their own licenses,
which are overwhelmingly MIT or Apache-2.0; `cargo tree` will list them if you
need the full picture.
