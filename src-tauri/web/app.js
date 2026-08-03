/* =========================================================
   LAN Share — receiver client
   Vanilla, single file, no framework, no build step.
   Sections:
     1  Constants + state
     2  Helpers (escape, format, DOM)
     3  Icons
     4  Toasts + banner
     5  API wrapper
     6  Router
     7  PIN gate
     8  Share picker
     9  Browser: fetch, render grid/list, chunked append
    10  Thumbnails: lazy load, concurrency + decode budgets
    11  Viewer (lightbox / player)
    12  Downloads + zip
    13  Upload sheet
    14  Boot + event wiring
   ========================================================= */

(function () {
  "use strict";

  // ============================================================
  // 1  Constants + state
  // ============================================================

  /** Rows/tiles appended per frame. Small enough that a 5,000-entry folder
   *  never blocks the main thread for a visible beat. */
  const CHUNK = 120;

  /** Concurrent thumbnail requests. HTTP/1.1 allows 6 per origin anyway; the
   *  semaphore matters because it lets us REORDER — a queued tile that scrolled
   *  away is dropped before its request ever starts. */
  const THUMB_PARALLEL = 6;

  /** Decoded thumbnails held at once. 320 x 200x200 RGBA is ~51 MB, safely
   *  inside iOS Safari's per-tab ceiling. Without this cap, a 5,000-image
   *  folder reaches ~800 MB and the tab is killed with no error. */
  const THUMB_BUDGET = 320;

  /** Load a tile's thumbnail before it is visible, and unload well after it
   *  leaves, so ordinary scroll jitter never thrashes the same image. */
  const THUMB_MARGIN = "700px 0px";

  const state = {
    session: null,
    shares: [],
    // Current listing from /api/list.
    listing: null,
    // The filtered + sorted view of listing.entries that is actually rendered.
    visible: [],
    rendered: 0,
    view: "grid",
    sort: "name",
    filter: "",
    // Files only, in render order — the viewer steps through this.
    viewable: [],
    viewIndex: -1,
    uploadQueue: [],
    uploading: false,
    online: true,
    // Guards against a slow response for folder A landing after the user has
    // already navigated to folder B.
    requestSeq: 0,
  };

  // Thumbnail scheduler state.
  const thumbs = {
    queue: [],
    active: 0,
    loaded: [],
    observer: null,
  };

  const $ = (id) => document.getElementById(id);

  // ============================================================
  // 2  Helpers
  // ============================================================

  /** Every filename goes through this before touching innerHTML. Filenames are
   *  attacker-influenced the moment the upload inbox is enabled. */
  function escapeHtml(value) {
    return String(value == null ? "" : value)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function formatBytes(size) {
    const units = ["B", "KB", "MB", "GB"];
    let value = Number(size) || 0;
    for (const unit of units) {
      if (value < 1024 || unit === "GB") return value.toFixed(1) + " " + unit;
      value /= 1024;
    }
    return size + " B";
  }

  function formatDate(ms) {
    if (!ms) return "";
    const d = new Date(Number(ms));
    if (Number.isNaN(d.getTime())) return "";
    const now = new Date();
    const sameYear = d.getFullYear() === now.getFullYear();
    return d.toLocaleDateString(undefined, {
      year: sameYear ? undefined : "numeric",
      month: "short",
      day: "numeric",
    });
  }

  function debounce(fn, ms) {
    let timer = null;
    return function (...args) {
      clearTimeout(timer);
      timer = setTimeout(() => fn.apply(null, args), ms);
    };
  }

  // --- URL builders. The server no longer ships per-entry URLs; deriving them
  // client-side cuts a 5,000-entry listing payload by roughly two thirds.
  const enc = encodeURIComponent;

  function joinPath(base, name) {
    return base ? base + "/" + name : name;
  }
  function urlStream(shareId, path) {
    return "/files/" + enc(shareId) + "/" + path.split("/").map(enc).join("/");
  }
  function urlDownload(shareId, path) {
    return "/download/" + enc(shareId) + "/" + path.split("/").map(enc).join("/");
  }
  function urlZip(shareId, path) {
    return "/zip/" + enc(shareId) + "/" + path.split("/").map(enc).join("/");
  }

  // ============================================================
  // 3  Icons — inline SVG paths, no icon font, no network request
  // ============================================================

  const ICONS = {
    folder: '<path d="M4 20h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.5l-2-2H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2Z"/>',
    image: '<rect x="3" y="3" width="18" height="18" rx="2"/><circle cx="9" cy="9" r="2"/><path d="m21 15-4.5-4.5L3 21"/>',
    video: '<rect x="2" y="5" width="14" height="14" rx="2"/><path d="m22 8-6 4 6 4V8Z"/>',
    audio: '<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="18" cy="16" r="3"/>',
    pdf: '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8Z"/><path d="M14 2v6h6"/>',
    text: '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8Z"/><path d="M14 2v6h6"/><path d="M8 13h8M8 17h5"/>',
    archive: '<rect x="2" y="4" width="20" height="5" rx="1"/><path d="M4 9v10a1 1 0 0 0 1 1h14a1 1 0 0 0 1-1V9"/><path d="M10 13h4"/>',
    other: '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8Z"/><path d="M14 2v6h6"/>',
    download: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m7 10 5 5 5-5"/><path d="M12 15V3"/>',
    upload: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m17 8-5-5-5 5"/><path d="M12 3v12"/>',
    back: '<path d="M19 12H5"/><path d="m12 19-7-7 7-7"/>',
    grid: '<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/>',
    list: '<path d="M8 6h13M8 12h13M8 18h13"/><path d="M3 6h.01M3 12h.01M3 18h.01"/>',
    search: '<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>',
    close: '<path d="M18 6 6 18M6 6l12 12"/>',
    play: '<path d="m5 3 14 9-14 9V3Z"/>',
    "chevron-left": '<path d="m15 18-6-6 6-6"/>',
    "chevron-right": '<path d="m9 18 6-6-6-6"/>',
    logout: '<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><path d="m16 17 5-5-5-5"/><path d="M21 12H9"/>',
  };

  function icon(name) {
    const body = ICONS[name] || ICONS.other;
    return '<svg viewBox="0 0 24 24" aria-hidden="true">' + body + "</svg>";
  }

  function fileIcon(kind) {
    return icon(ICONS[kind] ? kind : "other");
  }

  /** Fill every [data-icon] host that has not been filled yet. */
  function hydrateIcons(root) {
    (root || document).querySelectorAll("[data-icon]").forEach((el) => {
      if (!el.firstElementChild) el.innerHTML = icon(el.dataset.icon);
    });
  }

  // ============================================================
  // 4  Toasts + banner
  // ============================================================

  function toast(message, kind) {
    const host = $("toasts");
    const el = document.createElement("div");
    el.className = "toast" + (kind ? " toast-" + kind : "");
    el.textContent = message;
    host.appendChild(el);
    requestAnimationFrame(() => el.classList.add("show"));
    setTimeout(() => {
      el.classList.remove("show");
      setTimeout(() => el.remove(), 220);
    }, 3200);
  }

  function setBanner(message) {
    const banner = $("banner");
    if (!message) {
      banner.classList.add("hidden");
      return;
    }
    $("banner-text").textContent = message;
    banner.classList.remove("hidden");
  }

  // ============================================================
  // 5  API wrapper
  // ============================================================

  async function api(path, options) {
    const opts = Object.assign({ credentials: "same-origin" }, options || {});
    opts.headers = Object.assign(
      // Proves the request came from our own JS: a cross-site form post cannot
      // set a custom header. SameSite=Lax on the cookie is the first lock.
      { "X-LanShare": "1" },
      opts.headers || {}
    );

    let response;
    try {
      response = await fetch(path, opts);
    } catch (err) {
      state.online = false;
      setBanner("Lost connection to the host");
      throw new Error("offline");
    }

    if (!state.online) {
      state.online = true;
      setBanner("");
    }

    if (response.status === 401) {
      // The session expired or the host restarted (sessions are per-run).
      state.session = null;
      showPin();
      throw new Error("unauthorized");
    }

    if (!response.ok) {
      let detail = "";
      try {
        const body = await response.json();
        detail = body.detail || body.error || "";
      } catch (_e) {
        /* non-JSON error body */
      }
      const error = new Error(detail || "request failed (" + response.status + ")");
      error.status = response.status;
      throw error;
    }

    if (response.status === 204) return null;
    return response.json();
  }

  // ============================================================
  // 5b  Handing a file to a real media player
  // ============================================================
  //
  // No browser decodes Matroska, and none can be taught to: there is no codec
  // to install, and VLC is a player rather than something a page can borrow
  // from. What the viewer's device almost certainly does have is a player that
  // reads the file fine. So ask the server for a link and hand the stream over.
  //
  // The link is a play token, not our session: a player cannot hold a cookie.
  // It names one file and expires — see `models::PlayToken`.

  const IS_ANDROID = /Android/i.test(navigator.userAgent);
  const IS_IOS =
    /iPad|iPhone|iPod/.test(navigator.userAgent) ||
    // iPadOS 13+ reports itself as a Mac; the touch points give it away.
    (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);

  async function playLink(shareId, path) {
    const link = await api("/api/play/link", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ share: shareId, path }),
    });
    const name = encodeURIComponent(link.name).replace(/%2E/gi, ".");
    return {
      // Absolute: a player is a separate application and has no page to be
      // relative to.
      url: location.origin + "/play/" + link.token + "/" + name,
      playlist: location.origin + "/playlist/" + link.token,
      name: link.name,
    };
  }

  /// Send the file to whatever plays video on this device.
  ///
  /// Three routes, because the platforms genuinely differ:
  ///  - Android takes an intent URL, and shows a real app chooser. No `package=`
  ///    is pinned, so it offers VLC, MX Player or whatever is installed rather
  ///    than insisting on one, and `browser_fallback_url` catches a device with
  ///    none of them.
  ///  - iOS has no intent mechanism; VLC registers a callback scheme instead.
  ///  - Desktop has neither. No browser exposes an app chooser, so there is
  ///    nothing to call and nothing to fall back to — the sheet asks instead.
  async function openInPlayer(shareId, path) {
    let link;
    try {
      link = await playLink(shareId, path);
    } catch (err) {
      if (err.message !== "unauthorized") toast("Couldn't create a play link", "error");
      return;
    }

    if (IS_ANDROID) {
      const intent =
        "intent://" +
        link.url.replace(/^https?:\/\//, "") +
        "#Intent;scheme=http;action=android.intent.action.VIEW;type=video/*" +
        ";S.browser_fallback_url=" +
        encodeURIComponent(link.playlist) +
        ";end";
      location.href = intent;
      return;
    }

    if (IS_IOS) {
      // A top-level navigation, not a hidden iframe: our own CSP is
      // `default-src 'self'`, which frame-src inherits, so an iframe pointing at
      // a custom scheme is blocked before iOS ever sees it. A navigation is not
      // governed by CSP at all, and a scheme with no handler leaves the gallery
      // where it is — Safari just says it cannot open the address.
      toast("Opening in VLC — if nothing happens, use Copy link", "info");
      location.href =
        "vlc-x-callback://x-callback-url/stream?url=" + encodeURIComponent(link.url);
      return;
    }

    // Desktop. There is no chooser to call: no browser exposes one, and
    // navigating straight to the .m3u just drops a file into Downloads and
    // leaves the page looking like the button did nothing. Show what is on
    // offer instead and let the person pick.
    showPlaySheet(link);
  }

  function showPlaySheet(link) {
    $("play-file").textContent = link.name;
    $("play-url").value = link.url;
    $("play-sheet").dataset.playlist = link.playlist;
    $("play-sheet").classList.remove("hidden");
    // Pre-selected, so Ctrl+C works without touching the Copy button.
    const field = $("play-url");
    field.focus();
    field.setSelectionRange(0, field.value.length);
  }

  function closePlaySheet() {
    $("play-sheet").classList.add("hidden");
  }

  /// Put a string on the clipboard.
  ///
  /// `navigator.clipboard` needs a secure context and a plain-HTTP LAN address
  /// is not one, so the old selection route is not a nicety here -- on most of
  /// the devices this app runs on, it is the only one that works.
  async function copyToClipboard(value) {
    try {
      await navigator.clipboard.writeText(value);
      return true;
    } catch (_err) {
      /* fall through */
    }
    const field = document.createElement("textarea");
    field.value = value;
    field.setAttribute("readonly", "");
    field.style.position = "fixed";
    field.style.opacity = "0";
    document.body.appendChild(field);
    field.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch (_e) {
      ok = false;
    }
    field.remove();
    return ok;
  }

  async function copyPlayUrl() {
    const field = $("play-url");
    // Selected either way: if the copy fails there is at least something to
    // press Ctrl+C on.
    field.focus();
    field.setSelectionRange(0, field.value.length);
    if (await copyToClipboard(field.value)) {
      toast("Copied — paste it into VLC with Ctrl+N", "success");
    } else {
      toast("Copy failed — the link is selected, press Ctrl+C", "error");
    }
  }

  async function copyStreamLink(shareId, path) {
    let link;
    try {
      link = await playLink(shareId, path);
    } catch (err) {
      if (err.message !== "unauthorized") toast("Couldn't create a play link", "error");
      return;
    }
    if (await copyToClipboard(link.url)) {
      toast("Link copied — paste it into VLC → Open Network Stream", "success");
    } else {
      // Nowhere to paste from, so show the link instead of claiming success.
      showPlaySheet(link);
    }
  }

  // ============================================================
  // 6  Router — query strings, not path routes
  // ============================================================
  //
  // The server matches `GET /` exactly. A path route like /b/photos/2024 would
  // need a catch-all rewrite that must not shadow /files, /api, /s/{token} or
  // /zip. Query strings mean a hard reload of any deep link still hits `/` and
  // still gets the shell, with zero extra server routing.
  //
  // pushState is non-negotiable: on Android the system Back gesture is the only
  // back affordance, and without it the first back exits the site entirely from
  // three folders deep.

  function readRoute() {
    const params = new URLSearchParams(location.search);
    return {
      share: params.get("s") || "",
      path: params.get("p") || "",
      // `f` is the FILENAME, not an index — so a shared deep link survives a
      // re-sort, and a deleted neighbour does not shift the target.
      file: params.get("f") || "",
      view: params.get("v") || "",
    };
  }

  function routeUrl(route) {
    const params = new URLSearchParams();
    if (route.share) params.set("s", route.share);
    if (route.path) params.set("p", route.path);
    if (route.view && route.view !== "grid") params.set("v", route.view);
    if (route.file) params.set("f", route.file);
    const query = params.toString();
    return query ? "?" + query : location.pathname;
  }

  function navigate(route, replace) {
    const url = routeUrl(route);
    if (replace) history.replaceState(route, "", url);
    else history.pushState(route, "", url);
    render();
  }

  function currentRoute() {
    return readRoute();
  }

  window.addEventListener("popstate", render);

  // ============================================================
  // 7  PIN gate
  // ============================================================

  function showScreen(id) {
    ["screen-pin", "screen-shares", "screen-browse"].forEach((s) => {
      $(s).classList.toggle("hidden", s !== id);
    });
  }

  function showPin() {
    showScreen("screen-pin");
    $("pin-error").textContent = "";
    // autofocus would summon the keyboard over the layout on mobile before the
    // user has seen the screen, so focus only on a pointer-capable device.
    if (window.matchMedia("(pointer: fine)").matches) $("pin-input").focus();
  }

  let lockoutTimer = null;

  async function submitPin(event) {
    event.preventDefault();
    const input = $("pin-input");
    const button = $("pin-submit");
    const errorEl = $("pin-error");
    const pin = input.value.trim();

    if (!pin) {
      failPin("Enter the PIN");
      return;
    }

    button.disabled = true;
    button.textContent = "Connecting…";
    errorEl.textContent = "";

    try {
      const response = await fetch("/api/auth", {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json", "X-LanShare": "1" },
        body: JSON.stringify({ pin }),
      });

      if (response.ok) {
        input.value = "";
        await boot();
        return;
      }

      const body = await response.json().catch(() => ({}));
      if (response.status === 429) {
        startLockout(Number(body.retryAfterMs) || 30000);
      } else {
        const left = body.attemptsLeft;
        failPin(
          left != null
            ? "Wrong PIN — " + left + " attempt" + (left === 1 ? "" : "s") + " left"
            : "Wrong PIN"
        );
      }
    } catch (_err) {
      failPin("Could not reach the host");
    } finally {
      if (!lockoutTimer) {
        button.disabled = false;
        button.textContent = "Connect";
      }
    }
  }

  function failPin(message) {
    const card = document.querySelector(".pin-card");
    $("pin-error").textContent = message;
    $("pin-input").value = "";
    card.classList.remove("shake");
    // Force a reflow so the animation restarts on a second consecutive failure.
    void card.offsetWidth;
    card.classList.add("shake");
  }

  function startLockout(ms) {
    const button = $("pin-submit");
    const input = $("pin-input");
    let remaining = Math.ceil(ms / 1000);
    input.disabled = true;
    button.disabled = true;
    clearInterval(lockoutTimer);

    const tick = () => {
      $("pin-error").textContent = "Too many attempts — try again in " + remaining + "s";
      if (remaining <= 0) {
        clearInterval(lockoutTimer);
        lockoutTimer = null;
        input.disabled = false;
        button.disabled = false;
        button.textContent = "Connect";
        $("pin-error").textContent = "";
        return;
      }
      remaining -= 1;
    };
    tick();
    lockoutTimer = setInterval(tick, 1000);
  }

  // ============================================================
  // 8  Share picker
  // ============================================================

  async function loadShares() {
    try {
      state.shares = await api("/api/shares");
    } catch (err) {
      if (err.message === "unauthorized") return;
      state.shares = [];
    }
    renderShares();
  }

  function renderShares() {
    const list = $("shares-list");
    const empty = $("shares-empty");

    if (!state.shares.length) {
      list.innerHTML = "";
      empty.classList.remove("hidden");
      hydrateIcons(empty);
      return;
    }
    empty.classList.add("hidden");

    list.innerHTML = state.shares
      .map(
        (share) => `
        <button class="share-card" type="button" data-share="${escapeHtml(share.id)}">
          <span class="share-icon">${icon(share.is_file ? "other" : "folder")}</span>
          <span class="share-body">
            <span class="share-name">${escapeHtml(share.name)}</span>
            <span class="share-sub">${share.is_file ? "Single file" : "Folder"}</span>
          </span>
          ${share.is_inbox ? '<span class="tag tag-inbox">Inbox</span>' : ""}
        </button>`
      )
      .join("");
  }

  // ============================================================
  // 9  Browser
  // ============================================================

  async function loadListing(shareId, path) {
    const seq = ++state.requestSeq;
    showBrowseLoading(true);

    try {
      const listing = await api(
        "/api/list?share=" + enc(shareId) + "&path=" + enc(path) + "&sort=" + enc(state.sort)
      );
      // A slower response for a folder the user already left must not paint.
      if (seq !== state.requestSeq) return;
      state.listing = listing;
      applyFilterAndRender();
    } catch (err) {
      if (seq !== state.requestSeq) return;
      if (err.message === "unauthorized") return;
      state.listing = null;
      showBrowseLoading(false);
      showBrowseEmpty(
        err.status === 404 ? "Folder not found" : "Could not open this folder",
        err.message || "",
        null
      );
    }
  }

  function showBrowseLoading(active) {
    const skeleton = $("browse-loading");
    if (active) {
      if (!skeleton.childElementCount) {
        skeleton.innerHTML = Array.from({ length: 12 }, () => '<div class="sk"></div>').join("");
      }
      skeleton.classList.remove("hidden");
      $("grid").classList.add("hidden");
      $("list").classList.add("hidden");
      $("browse-empty").classList.add("hidden");
    } else {
      skeleton.classList.add("hidden");
    }
  }

  function showBrowseEmpty(title, sub, actionLabel) {
    const empty = $("browse-empty");
    const action = $("browse-empty-action");
    $("browse-empty-title").textContent = title;
    $("browse-empty-sub").textContent = sub || "";
    action.classList.toggle("hidden", !actionLabel);
    if (actionLabel) action.textContent = actionLabel;
    empty.classList.remove("hidden");
    $("grid").classList.add("hidden");
    $("list").classList.add("hidden");
    hydrateIcons(empty);
  }

  function applyFilterAndRender() {
    showBrowseLoading(false);
    const listing = state.listing;
    if (!listing) return;

    const needle = state.filter.trim().toLowerCase();
    state.visible = needle
      ? listing.entries.filter((e) => e.name.toLowerCase().includes(needle))
      : listing.entries.slice();

    state.viewable = state.visible.filter((e) => !e.is_dir);

    renderCrumbs();
    renderBottomBar();

    if (!state.visible.length) {
      // A filtered-empty folder needs different copy from a genuinely empty
      // one — otherwise the user thinks the folder is broken.
      if (needle) {
        showBrowseEmpty('No files match "' + state.filter + '"', "", "Clear filter");
      } else {
        showBrowseEmpty("This folder is empty", "", null);
      }
      return;
    }

    $("browse-empty").classList.add("hidden");
    renderEntries();
  }

  function renderEntries() {
    resetThumbs();
    state.rendered = 0;

    const grid = $("grid");
    const list = $("list");
    const isGrid = state.view === "grid";

    grid.innerHTML = "";
    list.innerHTML = "";
    grid.classList.toggle("hidden", !isGrid);
    list.classList.toggle("hidden", isGrid);

    appendChunk();
  }

  /** Append the next CHUNK entries. Called on first render and by the scroll
   *  sentinel. Chunked rather than virtualized: see the note on
   *  `content-visibility` in styles.css. */
  function appendChunk() {
    const container = state.view === "grid" ? $("grid") : $("list");
    const slice = state.visible.slice(state.rendered, state.rendered + CHUNK);
    if (!slice.length) return;

    const html = slice
      .map((entry) => (state.view === "grid" ? tileHtml(entry) : rowHtml(entry)))
      .join("");

    container.insertAdjacentHTML("beforeend", html);
    state.rendered += slice.length;

    // Observe only the newly added nodes.
    const fresh = Array.from(container.children).slice(-slice.length);
    fresh.forEach((node) => {
      hydrateIcons(node);
      const img = node.querySelector("img[data-thumb]");
      if (img && thumbs.observer) thumbs.observer.observe(img);
    });
  }

  function tileHtml(entry) {
    const shareId = state.listing.share_id;
    const rel = joinPath(state.listing.path, entry.name);
    const name = escapeHtml(entry.name);

    const thumbInner = entry.thumb_url
      ? `<img data-thumb="${escapeHtml(entry.thumb_url)}" alt="" loading="lazy" decoding="async" />`
      : `<span class="tile-glyph" data-icon="${entry.is_dir ? "folder" : entry.kind}"></span>`;

    // A video tile gets a play overlay rather than a <video preload="metadata">
    // poster: on mobile, N video elements each open a connection and decode a
    // header, which is far more expensive than the poster is worth.
    const overlay = entry.kind === "video" && !entry.is_dir ? '<span class="tile-play"></span>' : "";

    const badge = entry.is_dir
      ? ""
      : `<span class="tile-badge">${escapeHtml(entry.ext || entry.kind)}</span>`;

    return `
      <button class="tile ${entry.is_dir ? "tile-dir" : ""}" type="button" role="listitem"
              data-name="${name}" data-dir="${entry.is_dir ? "1" : "0"}" data-rel="${escapeHtml(rel)}">
        <span class="tile-thumb">${thumbInner}${overlay}${badge}</span>
        <span class="tile-meta">
          <span class="tile-name">${name}</span>
          <span class="tile-sub">${entry.is_dir ? "Folder" : escapeHtml(entry.size_text)}</span>
        </span>
      </button>`;
  }

  function rowHtml(entry) {
    const rel = joinPath(state.listing.path, entry.name);
    const name = escapeHtml(entry.name);
    const shareId = state.listing.share_id;

    const thumbInner = entry.thumb_url
      ? `<img data-thumb="${escapeHtml(entry.thumb_url)}" alt="" loading="lazy" decoding="async" />`
      : `<span data-icon="${entry.is_dir ? "folder" : entry.kind}"></span>`;

    const sub = entry.is_dir
      ? "Folder"
      : escapeHtml(entry.size_text) + (entry.mtime_ms ? " · " + escapeHtml(formatDate(entry.mtime_ms)) : "");

    // A real <a download> for files: on iOS Safari the download ATTRIBUTE is
    // ignored, but a same-origin navigation to a Content-Disposition:
    // attachment URL still saves the file — and unlike fetch+blob it never
    // buffers a 4 GB video in memory.
    const trailing = entry.is_dir
      ? ""
      : `<a class="row-dl" href="${escapeHtml(urlDownload(shareId, rel))}" download
             data-stop aria-label="Download ${name}"><span data-icon="download"></span></a>`;

    return `
      <div class="row ${entry.is_dir ? "row-dir" : ""}" role="listitem"
           data-name="${name}" data-dir="${entry.is_dir ? "1" : "0"}" data-rel="${escapeHtml(rel)}">
        <span class="row-thumb">${thumbInner}</span>
        <span class="row-body">
          <span class="row-name">${name}</span>
          <span class="row-sub">${sub}</span>
        </span>
        ${trailing}
      </div>`;
  }

  function renderCrumbs() {
    const listing = state.listing;
    const crumbs = $("crumbs");
    const parts = [
      { name: listing.share_name, path: "" },
      ...(listing.breadcrumbs || []),
    ];
    crumbs.innerHTML = parts
      .map(
        (c, i) =>
          (i ? '<span class="crumb-sep">/</span>' : "") +
          `<button class="crumb" type="button" data-crumb="${escapeHtml(c.path)}">${escapeHtml(c.name)}</button>`
      )
      .join("");
    // Keep the deepest crumb visible when the trail overflows.
    crumbs.scrollLeft = crumbs.scrollWidth;
  }

  function renderBottomBar() {
    const listing = state.listing;
    const parts = [];
    if (listing.dir_count) parts.push(listing.dir_count + " folder" + (listing.dir_count === 1 ? "" : "s"));
    if (listing.file_count) parts.push(listing.file_count + " file" + (listing.file_count === 1 ? "" : "s"));
    if (listing.total_bytes) parts.push(formatBytes(listing.total_bytes));
    $("folder-stat").textContent = parts.join(" · ");

    const canZip = state.session && state.session.allowZip && listing.file_count > 0;
    $("zip-btn").classList.toggle("hidden", !canZip);
    $("upload-btn").classList.toggle("hidden", !listing.can_upload);
  }

  // ============================================================
  // 10  Thumbnails — lazy, with a request cap and a decode budget
  // ============================================================

  function resetThumbs() {
    if (thumbs.observer) thumbs.observer.disconnect();
    thumbs.queue = [];
    thumbs.active = 0;
    thumbs.loaded = [];

    thumbs.observer = new IntersectionObserver(
      (entries) => {
        entries.forEach((record) => {
          const img = record.target;
          if (record.isIntersecting) {
            if (!img.dataset.thumb || img.src) return;
            if (!thumbs.queue.includes(img)) thumbs.queue.push(img);
          } else {
            // Only fires once the tile is THUMB_MARGIN outside the viewport, so
            // small scroll jitter never unloads what is about to come back.
            const idx = thumbs.queue.indexOf(img);
            if (idx >= 0) thumbs.queue.splice(idx, 1);
          }
        });
        pumpThumbs();
      },
      { rootMargin: THUMB_MARGIN }
    );
  }

  function pumpThumbs() {
    while (thumbs.active < THUMB_PARALLEL && thumbs.queue.length) {
      const img = thumbs.queue.shift();
      if (!img || img.src || !img.isConnected) continue;

      thumbs.active += 1;
      const done = () => {
        thumbs.active -= 1;
        pumpThumbs();
      };
      img.addEventListener("load", () => {
        registerLoaded(img);
        done();
      }, { once: true });
      img.addEventListener("error", () => {
        // A thumbnail the server could not produce: fall back to the type
        // glyph rather than leaving a broken-image box.
        const host = img.parentElement;
        if (host) {
          const glyph = document.createElement("span");
          glyph.className = "tile-glyph";
          glyph.innerHTML = icon("image");
          img.replaceWith(glyph);
        }
        done();
      }, { once: true });

      img.src = img.dataset.thumb;
    }
  }

  function registerLoaded(img) {
    thumbs.loaded.push(img);
    while (thumbs.loaded.length > THUMB_BUDGET) {
      const old = thumbs.loaded.shift();
      // Only drop images that have scrolled well away; a still-visible one
      // would visibly blank out.
      if (old && old.isConnected && !isNearViewport(old)) {
        old.removeAttribute("src");
      }
    }
  }

  function isNearViewport(el) {
    const rect = el.getBoundingClientRect();
    return rect.bottom > -700 && rect.top < window.innerHeight + 700;
  }

  // ============================================================
  // 11  Viewer
  // ============================================================

  function openViewer(name) {
    const index = state.viewable.findIndex((e) => e.name === name);
    if (index < 0) return;
    state.viewIndex = index;
    const route = currentRoute();
    // A pushState per open means Back closes the viewer before it navigates —
    // exactly one back-press to dismiss a fullscreen photo, as users expect.
    navigate({ ...route, file: name });
  }

  function closeViewer() {
    const route = currentRoute();
    if (route.file) {
      history.back();
    } else {
      $("viewer").classList.add("hidden");
    }
  }

  function stepViewer(delta) {
    if (state.viewIndex < 0) return;
    const next = state.viewIndex + delta;
    if (next < 0 || next >= state.viewable.length) return;
    state.viewIndex = next;
    const route = currentRoute();
    // replace, not push: paging through a gallery must not stack 200 history
    // entries the user then has to back out of one at a time.
    navigate({ ...route, file: state.viewable[next].name }, true);
  }

  function renderViewer(fileName) {
    const viewer = $("viewer");
    if (!fileName || !state.listing) {
      viewer.classList.add("hidden");
      $("viewer-stage").innerHTML = "";
      return;
    }

    const entry = state.listing.entries.find((e) => e.name === fileName && !e.is_dir);
    if (!entry) {
      viewer.classList.add("hidden");
      return;
    }

    state.viewIndex = state.viewable.findIndex((e) => e.name === fileName);

    const shareId = state.listing.share_id;
    const rel = joinPath(state.listing.path, entry.name);
    const streamUrl = urlStream(shareId, rel);
    const downloadUrl = urlDownload(shareId, rel);

    $("viewer-name").textContent = entry.name;
    $("viewer-download").href = downloadUrl;
    $("viewer-download").setAttribute("download", entry.name);
    $("viewer-meta").textContent =
      entry.size_text + (entry.mtime_ms ? " · " + formatDate(entry.mtime_ms) : "");

    const stage = $("viewer-stage");
    // What the play buttons act on. Read at click time rather than closed over,
    // so stepping to the next file with the arrows cannot leave a stale target
    // behind on a button that was never re-rendered.
    stage.dataset.playShare = shareId;
    stage.dataset.playPath = rel;

    const isMedia = entry.kind === "video" || entry.kind === "audio";

    if (!entry.playable && isMedia) {
      // .mkv, .avi, .wmv: the file is fine, the software looking at it is the
      // wrong software. Nothing can be installed into a browser to fix that, so
      // offer the one thing that does work — the player already on this device.
      stage.innerHTML = `
        <div class="viewer-fallback">
          <div class="empty-icon">${fileIcon(entry.kind)}</div>
          <p class="empty-title" style="color:#f4f4f5">Browsers can't play ${escapeHtml(entry.ext.toUpperCase())}</p>
          <p class="empty-sub">Open it in VLC or another player instead — it streams straight from the host, nothing is downloaded first.</p>
          <div class="viewer-actions">
            <button class="btn btn-primary" type="button" data-play="open">Open in player</button>
            <button class="btn" type="button" data-play="copy">Copy link</button>
            <a class="btn" href="${escapeHtml(downloadUrl)}" download>Download</a>
          </div>
        </div>`;
    } else if (!entry.playable) {
      stage.innerHTML = `
        <div class="viewer-fallback">
          <div class="empty-icon">${fileIcon(entry.kind)}</div>
          <p class="empty-title" style="color:#f4f4f5">Can't open this here</p>
          <p class="empty-sub">${escapeHtml(entry.ext.toUpperCase())} isn't something a browser can show.</p>
          <a class="btn btn-primary" href="${escapeHtml(downloadUrl)}" download>Download</a>
        </div>`;
    } else if (entry.kind === "image") {
      stage.innerHTML = `<img src="${escapeHtml(streamUrl)}" alt="${escapeHtml(entry.name)}" />`;
    } else if (entry.kind === "video") {
      // playsinline or iOS hijacks playback into its own fullscreen player.
      // preload="metadata" gets the duration for the scrubber without pulling
      // the file. Seeking then issues Range requests the server answers with 206.
      //
      // The hand-off stays on offer even here: the container is one a browser
      // reads, but the codecs inside may not be (HEVC, AC3), and a phone often
      // handles a big file better in a real player.
      stage.innerHTML = `
        <div class="viewer-media">
          <video src="${escapeHtml(streamUrl)}" controls playsinline preload="metadata" autoplay></video>
          <div class="viewer-actions viewer-actions-under">
            <button class="btn btn-sm" type="button" data-play="open">Open in player</button>
            <button class="btn btn-sm" type="button" data-play="copy">Copy link</button>
          </div>
        </div>`;
    } else if (entry.kind === "audio") {
      stage.innerHTML = `<audio src="${escapeHtml(streamUrl)}" controls preload="metadata" autoplay></audio>`;
    } else {
      stage.innerHTML = `
        <div class="viewer-fallback">
          <div class="empty-icon">${fileIcon(entry.kind)}</div>
          <p class="empty-title" style="color:#f4f4f5">${escapeHtml(entry.name)}</p>
          <a class="btn btn-primary" href="${escapeHtml(downloadUrl)}" download>Download</a>
        </div>`;
    }

    $("viewer-prev").classList.toggle("hidden", state.viewIndex <= 0);
    $("viewer-next").classList.toggle("hidden", state.viewIndex >= state.viewable.length - 1);
    viewer.classList.remove("hidden");
    hydrateIcons(viewer);
  }

  // --- swipe, the primary gesture on a phone
  let touchStartX = 0;
  let touchStartY = 0;

  function onViewerTouchStart(event) {
    touchStartX = event.changedTouches[0].clientX;
    touchStartY = event.changedTouches[0].clientY;
  }

  function onViewerTouchEnd(event) {
    const dx = event.changedTouches[0].clientX - touchStartX;
    const dy = event.changedTouches[0].clientY - touchStartY;
    // Horizontal intent only, and a threshold high enough that a pinch-zoom
    // pan does not page to the next image.
    if (Math.abs(dx) > 60 && Math.abs(dx) > Math.abs(dy) * 1.5) {
      stepViewer(dx < 0 ? 1 : -1);
    }
  }

  // ============================================================
  // 12  Downloads + zip
  // ============================================================

  function downloadFolder() {
    if (!state.listing) return;
    const url = urlZip(state.listing.share_id, state.listing.path);
    toast("Preparing archive…");
    // A plain navigation: the response is chunked with no Content-Length, so
    // fetch+blob would buffer the whole archive in memory before saving.
    window.location.href = url;
  }

  // ============================================================
  // 13  Upload sheet
  // ============================================================

  function openUploadSheet() {
  // Framed as sending TO a named device rather than "uploading to a folder":
  // to the person holding the phone this is the same act as a desktop peer
  // sending them a file, and it should read that way.
  const host = state.session && state.session.serverName;
  $("upload-title").textContent = host ? "Send files to " + host : "Send files";

    state.uploadQueue = [];
    renderUploadQueue();
    $("upload-input").value = "";
    $("upload-start").disabled = true;
    $("upload-sheet").classList.remove("hidden");
    hydrateIcons($("upload-sheet"));
  }

  function closeUploadSheet() {
    $("upload-sheet").classList.add("hidden");
  }

  function onUploadPick(event) {
    state.uploadQueue = Array.from(event.target.files || []).map((file) => ({
      file,
      status: "queued",
      percent: 0,
      message: "",
    }));
    renderUploadQueue();
    $("upload-start").disabled = !state.uploadQueue.length;
  }

  function renderUploadQueue() {
    $("upload-queue").innerHTML = state.uploadQueue
      .map(
        (job) => `
        <div class="upload-item ${job.status === "error" ? "is-error" : ""} ${job.status === "done" ? "is-done" : ""}">
          <div class="upload-item-head">
            <span class="upload-item-name">${escapeHtml(job.file.name)}</span>
            <span class="upload-item-status">${escapeHtml(job.message || formatBytes(job.file.size))}</span>
          </div>
          <div class="progress-track"><div class="progress-bar" style="width:${job.percent}%"></div></div>
        </div>`
      )
      .join("");
  }

  async function startUpload() {
    if (state.uploading || !state.listing) return;
    state.uploading = true;
    $("upload-start").disabled = true;

    for (const job of state.uploadQueue) {
      if (job.status === "done") continue;
      job.status = "uploading";
      try {
        await uploadOne(job);
        job.status = "done";
        job.percent = 100;
        job.message = "Uploaded";
      } catch (err) {
        job.status = "error";
        job.message = err.message || "Failed";
      }
      renderUploadQueue();
    }

    state.uploading = false;
    const failed = state.uploadQueue.filter((j) => j.status === "error").length;
    if (failed) toast(failed + " file" + (failed === 1 ? "" : "s") + " failed", "error");
    else toast("Upload complete", "success");

    closeUploadSheet();
    loadListing(state.listing.share_id, state.listing.path);
  }

  /** XMLHttpRequest rather than fetch: `fetch` has no upload progress event,
   *  and a progress bar is the whole point of a mobile upload UI. One file per
   *  request, sequentially, so each bar reflects one real transfer. */
  function uploadOne(job) {
    return new Promise((resolve, reject) => {
      const form = new FormData();
      form.append("file", job.file, job.file.name);

      const url =
        "/api/upload?share=" + enc(state.listing.share_id) + "&path=" + enc(state.listing.path);

      const xhr = new XMLHttpRequest();
      xhr.open("POST", url);
      xhr.withCredentials = true;
      xhr.setRequestHeader("X-LanShare", "1");

      xhr.upload.onprogress = (event) => {
        if (!event.lengthComputable) return;
        job.percent = Math.round((event.loaded / event.total) * 100);
        job.message = job.percent + "%";
        renderUploadQueue();
      };

      xhr.onload = () => {
        if (xhr.status === 413) return reject(new Error("Too large"));
        if (xhr.status === 403) return reject(new Error("Not allowed"));
        if (xhr.status < 200 || xhr.status >= 300) return reject(new Error("Failed"));
        let body = {};
        try {
          body = JSON.parse(xhr.responseText);
        } catch (_e) {
          /* tolerate a non-JSON success body */
        }
        if (body.rejected && body.rejected.length) {
          return reject(new Error(body.rejected[0].reason || "Rejected"));
        }
        resolve();
      };
      xhr.onerror = () => reject(new Error("Network error"));
      xhr.send(form);
    });
  }

  // ============================================================
  // 14  Render dispatcher, events, boot
  // ============================================================

  function render() {
    const route = currentRoute();

    if (!state.session || !state.session.authenticated) {
      showPin();
      return;
    }

    if (!route.share) {
      // A share-scoped session sees exactly one share, so skip the picker.
      if (state.session.shareId) {
        navigate({ share: state.session.shareId, path: "", view: state.view }, true);
        return;
      }
      showScreen("screen-shares");
      renderViewer(null);
      loadShares();
      return;
    }

    showScreen("screen-browse");
    if (route.view) state.view = route.view;
    updateViewButton();

    const listing = state.listing;
    const sameFolder =
      listing && listing.share_id === route.share && listing.path === route.path;

    if (!sameFolder) {
      state.filter = "";
      $("search-input").value = "";
      loadListing(route.share, route.path).then(() => renderViewer(route.file));
    } else {
      renderViewer(route.file);
    }
  }

  function updateViewButton() {
    const btn = $("view-btn");
    btn.dataset.icon = state.view === "grid" ? "list" : "grid";
    btn.innerHTML = icon(btn.dataset.icon);
  }

  function goBack() {
    const route = currentRoute();
    if (!route.path) {
      // At a share root: back to the picker, unless the session is scoped to
      // this one share, in which case there is nowhere above to go.
      if (state.session && state.session.shareId) return;
      navigate({ share: "", path: "", view: state.view });
      return;
    }
    const parent = route.path.includes("/")
      ? route.path.slice(0, route.path.lastIndexOf("/"))
      : "";
    navigate({ share: route.share, path: parent, view: state.view });
  }

  function onEntryActivate(target) {
    const isDir = target.dataset.dir === "1";
    const name = target.dataset.name;
    const route = currentRoute();
    if (isDir) {
      navigate({ share: route.share, path: joinPath(route.path, name), view: state.view });
    } else {
      openViewer(name);
    }
  }

  function wire() {
    hydrateIcons(document);
    updateViewButton();

    $("pin-form").addEventListener("submit", submitPin);
    // Digits only — a numeric keypad still lets a paste bring in anything.
    $("pin-input").addEventListener("input", (e) => {
      e.target.value = e.target.value.replace(/\D/g, "");
    });

    $("banner-retry").addEventListener("click", () => {
      setBanner("");
      boot();
    });

    $("shares-list").addEventListener("click", (event) => {
      const card = event.target.closest("[data-share]");
      if (!card) return;
      navigate({ share: card.dataset.share, path: "", view: state.view });
    });

    document.querySelector('[data-action="reload-shares"]').addEventListener("click", loadShares);

    $("back-btn").addEventListener("click", goBack);

    $("crumbs").addEventListener("click", (event) => {
      const crumb = event.target.closest("[data-crumb]");
      if (!crumb) return;
      navigate({ share: currentRoute().share, path: crumb.dataset.crumb, view: state.view });
    });

    $("view-btn").addEventListener("click", () => {
      state.view = state.view === "grid" ? "list" : "grid";
      updateViewButton();
      const route = currentRoute();
      navigate({ ...route, view: state.view }, true);
      applyFilterAndRender();
    });

    $("search-btn").addEventListener("click", () => {
      const row = $("search-row");
      const nowHidden = row.classList.toggle("hidden");
      if (!nowHidden) $("search-input").focus();
      else if (state.filter) {
        state.filter = "";
        $("search-input").value = "";
        applyFilterAndRender();
      }
    });

    $("search-input").addEventListener(
      "input",
      debounce((event) => {
        state.filter = event.target.value;
        applyFilterAndRender();
      }, 160)
    );

    $("sort-select").addEventListener("change", (event) => {
      state.sort = event.target.value;
      const route = currentRoute();
      loadListing(route.share, route.path);
    });

    // One delegated listener for both containers — with 5,000 entries, per-node
    // listeners would be 5,000 closures.
    ["grid", "list"].forEach((id) => {
      $(id).addEventListener("click", (event) => {
        // Let the download anchor do its own navigation.
        if (event.target.closest("[data-stop]")) return;
        const target = event.target.closest("[data-name]");
        if (target) onEntryActivate(target);
      });
    });

    $("browse-empty-action").addEventListener("click", () => {
      state.filter = "";
      $("search-input").value = "";
      applyFilterAndRender();
    });

    $("zip-btn").addEventListener("click", downloadFolder);
    $("upload-btn").addEventListener("click", openUploadSheet);
    $("upload-close").addEventListener("click", closeUploadSheet);
    $("upload-input").addEventListener("change", onUploadPick);
    $("upload-start").addEventListener("click", startUpload);
    $("upload-sheet").addEventListener("click", (event) => {
      if (event.target === $("upload-sheet")) closeUploadSheet();
    });

    $("play-close").addEventListener("click", closePlaySheet);
    $("play-copy").addEventListener("click", copyPlayUrl);
    $("play-playlist").addEventListener("click", () => {
      // A navigation rather than fetch+blob: the response is an attachment, so
      // the browser saves it with the right name and the right handler.
      location.href = $("play-sheet").dataset.playlist;
      toast("Saved — click it in your downloads to start playing", "info");
    });
    $("play-sheet").addEventListener("click", (event) => {
      if (event.target === $("play-sheet")) closePlaySheet();
    });

    // Delegated, because the stage is rewritten on every file.
    $("viewer-stage").addEventListener("click", (event) => {
      const button = event.target.closest("[data-play]");
      if (!button) return;
      const stage = $("viewer-stage");
      const share = stage.dataset.playShare;
      const path = stage.dataset.playPath;
      if (!share || !path) return;
      if (button.dataset.play === "copy") copyStreamLink(share, path);
      else openInPlayer(share, path);
    });

    $("viewer-close").addEventListener("click", closeViewer);
    $("viewer-prev").addEventListener("click", () => stepViewer(-1));
    $("viewer-next").addEventListener("click", () => stepViewer(1));
    $("viewer-stage").addEventListener("touchstart", onViewerTouchStart, { passive: true });
    $("viewer-stage").addEventListener("touchend", onViewerTouchEnd, { passive: true });

    $("logout-btn").addEventListener("click", async () => {
      try {
        await api("/api/logout", { method: "POST" });
      } catch (_e) {
        /* already gone */
      }
      state.session = null;
      showPin();
    });

    document.addEventListener("keydown", (event) => {
      // The play sheet sits on top of the viewer, so it takes Escape first --
      // and the arrows must not page the gallery underneath while the link is
      // selected for copying.
      if (!$("play-sheet").classList.contains("hidden")) {
        if (event.key === "Escape") closePlaySheet();
        return;
      }
      if ($("viewer").classList.contains("hidden")) return;
      if (event.key === "Escape") closeViewer();
      else if (event.key === "ArrowLeft") stepViewer(-1);
      else if (event.key === "ArrowRight") stepViewer(1);
    });

    // Infinite scroll: append the next chunk as the sentinel nears the viewport.
    new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) appendChunk();
      },
      { rootMargin: "600px 0px" }
    ).observe($("sentinel"));

    window.addEventListener("online", () => {
      setBanner("");
      boot();
    });
    window.addEventListener("offline", () => setBanner("No network connection"));
  }

  async function boot() {
    try {
      state.session = await api("/api/session");
    } catch (_err) {
      state.session = null;
      setBanner("Can't reach the host");
      showPin();
      return;
    }

    setBanner("");

    if (!state.session.authenticated) {
      $("pin-host").textContent = state.session.serverName
        ? "Enter the PIN shown on " + state.session.serverName
        : "Enter the PIN shown on the host";
      showPin();
      return;
    }

    $("server-name").textContent = state.session.serverName || "LAN Share";
    if (state.session.defaultView) state.view = state.session.defaultView;
    if (state.session.defaultSort) {
      state.sort = state.session.defaultSort;
      $("sort-select").value = state.sort;
    }
    updateViewButton();
    render();
  }

  wire();
  boot();
})();
