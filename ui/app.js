/* =========================================================
   LAN Share — desktop control panel
   Vanilla JS, no bundler, no framework. Talks to Rust via
   window.__TAURI__.core.invoke.

   Sections:
     1  Runtime handle + constants + state
     2  DOM / format helpers
     3  Logging + toasts + confirm dialog
     4  Backend bridge
     5  Config
     6  Theme
     7  Page routing
     8  Server control
     9  Clipboard + QR
    10  Dashboard render
    11  Shares — data
    12  Shares — render
    13  Shares — index task
    15  Activity page
    16  Settings page
    17  Event wiring + boot
    18  Devices — discovery and pairing
    19  Network — browsing another device's shares
    20  Tooltips
   ========================================================= */

// ============================================================
// 1  Runtime handle + constants + state
// ============================================================

const invoke = window.__TAURI__?.core?.invoke;

const TASK_POLL_MS = 300;
const SERVER_POLL_MS = 1000;
const ACTIVITY_POLL_MS = 1500;
const ACTIVITY_LIMIT = 300;

const PAGES = ["dashboard", "shares", "network", "devices", "activity", "settings"];

const THEMES = [
  "light", "dark", "nord", "solarized", "monolith", "amber",
  "emerald", "midnight", "cyber", "porcelain", "frost", "circuit",
];

const THEME_LABELS = {
  light: "Light", dark: "Dark", nord: "Nord", solarized: "Solarized",
  monolith: "Monolith", amber: "Amber", emerald: "Emerald", midnight: "Midnight",
  cyber: "Cyber", porcelain: "Porcelain", frost: "Frost", circuit: "Circuit",
};

const state = {
  theme: "dark",
  currentPage: "dashboard",
  // Last AppConfig from load_config. Never mutated in place — Settings builds
  // a fresh object on save, so a failed save leaves this copy intact.
  config: null,
  logs: [],
  booted: false,

  server: {
    running: false,
    // Latched while start_server / stop_server is in flight. Drives the
    // toggle's disabled + .is-busy state so a double-click can't fire twice.
    transitioning: false,
    port: 8080,
    urls: [],
    urlIndex: 0,
    startedAtMs: 0,
    bytesServedText: "0.0 B",
    sessionCount: 0,
    pollTimer: null,
    // Last start_server rejection, kept so the Dashboard alert survives a
    // re-render. Cleared on the next successful start.
    lastError: null,
  },

  shares: {
    items: [],
    filter: "",
    // shareId -> { taskId, message, running }. Lives OUTSIDE `items` so a
    // list_shares refresh mid-index doesn't wipe the row progress bars.
    indexing: {},
    // Counts from the last index run, also kept outside `items`.
    stats: {},
    revealed: {},
    editing: null,
  },

  activity: {
    entries: [],
    filter: "all",
    autoRefresh: true,
    pollTimer: null,
    lastId: 0,
  },

  pinRevealed: false,
  qrCache: {},
  pendingConfirm: null,
};

// ============================================================
// 2  DOM / format helpers
// ============================================================

const $ = (id) => document.getElementById(id);

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

function formatCount(n, singular, plural) {
  const value = Number(n) || 0;
  return value + " " + (value === 1 ? singular : plural || singular + "s");
}

function formatUptime(ms) {
  const total = Math.floor((Number(ms) || 0) / 1000);
  if (total <= 0) return "—";
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h) return h + "h " + m + "m";
  if (m) return m + "m " + s + "s";
  return s + "s";
}

function formatClock(ms) {
  if (!ms) return "";
  const d = new Date(Number(ms));
  return Number.isNaN(d.getTime())
    ? ""
    : d.toLocaleTimeString(undefined, { hour12: false });
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function debounce(fn, ms) {
  let timer = null;
  return function (...args) {
    clearTimeout(timer);
    timer = setTimeout(() => fn.apply(null, args), ms);
  };
}

function parseIntOr(value, fallback) {
  const n = parseInt(String(value).replace(/[^0-9]/g, ""), 10);
  return Number.isFinite(n) ? n : fallback;
}

function parseExtList(value) {
  return String(value || "")
    .split(",")
    .map((s) => s.trim().replace(/^\./, "").toLowerCase())
    .filter(Boolean);
}

// ============================================================
// 3  Logging + toasts + confirm dialog
// ============================================================

function addLog(message) {
  const line = "[" + new Date().toLocaleTimeString(undefined, { hour12: false }) + "] " + message;
  state.logs.unshift(line);
  if (state.logs.length > 500) state.logs.length = 500;
  const body = $("console-body");
  if (body) {
    body.innerHTML = state.logs
      .map((l) => '<div class="log-item">' + escapeHtml(l) + "</div>")
      .join("");
  }
}

const TOAST_ICONS = { info: "info", success: "check_circle", error: "error" };

function showToast(message, kind) {
  const type = kind || "info";
  const host = $("toast-host");
  const el = document.createElement("div");
  el.className = "toast toast-" + type;
  el.innerHTML =
    '<span class="material-symbols-outlined toast-icon">' + TOAST_ICONS[type] + "</span>" +
    '<span class="toast-text">' + escapeHtml(message) + "</span>";
  host.appendChild(el);
  requestAnimationFrame(() => el.classList.add("show"));
  setTimeout(() => {
    el.classList.remove("show");
    setTimeout(() => el.remove(), 250);
  }, 3600);
}

/** Promise-based confirm. Resolves true on OK, false on Cancel or backdrop. */
function confirmDialog(title, body, okLabel) {
  return new Promise((resolve) => {
    $("confirm-title").textContent = title;
    $("confirm-body").textContent = body || "";
    $("confirm-ok").textContent = okLabel || "Confirm";
    $("confirm-backdrop").classList.remove("hidden");
    state.pendingConfirm = resolve;
  });
}

function closeConfirm(result) {
  $("confirm-backdrop").classList.add("hidden");
  const resolve = state.pendingConfirm;
  state.pendingConfirm = null;
  if (resolve) resolve(result);
}

// ============================================================
// 4  Backend bridge
// ============================================================

/** Invoke with a toast + log on failure. Rethrows so callers can still branch. */
async function call(command, args) {
  if (!invoke) throw new Error("Tauri bridge unavailable");
  try {
    return await invoke(command, args || {});
  } catch (err) {
    const message = typeof err === "string" ? err : err?.message || String(err);
    addLog(command + " failed: " + message);
    showToast(message, "error");
    throw new Error(message);
  }
}

/** Same, but silent — for polls, where a toast per tick would be unusable. */
async function callQuiet(command, args) {
  if (!invoke) return null;
  try {
    return await invoke(command, args || {});
  } catch (_err) {
    return null;
  }
}

/** Start a background task and poll it to completion. */
async function runTask(command, args, onProgress) {
  const handle = await call(command, args);
  const taskId = handle && handle.id != null ? handle.id : null;
  if (taskId == null) throw new Error("failed to start task");

  try {
    while (true) {
      await sleep(TASK_POLL_MS);
      const payload = await callQuiet("get_task_progress", { taskId });
      if (!payload) throw new Error("lost contact with the task");
      if (onProgress) onProgress(payload);
      if (payload.error) throw new Error(String(payload.error));
      if (payload.done) return payload;
    }
  } finally {
    await callQuiet("clear_task", { taskId });
  }
}

// ============================================================
// 5  Config
// ============================================================

async function loadConfig() {
  state.config = await callQuiet("load_config");
  if (!state.config) {
    showToast("Could not load settings", "error");
    return;
  }
  state.theme = state.config.theme || "dark";
  applyTheme(state.theme);
  applyConfigToUi();
}

function applyConfigToUi() {
  const c = state.config;
  if (!c) return;

  $("set-port").value = c.port;
  $("set-bind").value = c.bind_address;
  $("set-name").value = c.server_name || "";
  $("set-autoport").checked = !!c.auto_port;
  $("set-autostart").checked = !!c.autostart_server;
  $("set-keepawake").checked = !!c.keep_awake;
  $("set-hostcheck").checked = !!c.strict_host_check;

  $("set-pin-enabled").checked = !!c.pin_enabled;
  $("set-pin").value = c.pin || "";
  $("set-attempts").value = c.max_pin_attempts;
  $("set-lockout").value = c.lockout_seconds;
  $("set-ttl").value = c.session_ttl_hours;

  $("set-uploads").checked = !!c.uploads_enabled;
  $("set-maxupload").value = c.max_upload_mb;

  $("set-thumbs").checked = !!c.thumbnails_enabled;
  $("set-zip").checked = !!c.allow_folder_zip;
  $("set-hidden").checked = !!c.show_hidden;
  $("set-thumbsize").value = c.thumb_max_edge;
  $("set-view").value = c.default_view_mode;
  $("set-player").value = c.external_player || "";

  renderInboxOptions();
}

/** Build a fresh config object from the form. Never mutates `state.config`, so
 *  a rejected save leaves the in-memory copy usable. */
function readConfigFromUi() {
  const c = Object.assign({}, state.config);
  c.port = parseIntOr($("set-port").value, 8080);
  c.bind_address = $("set-bind").value;
  c.server_name = $("set-name").value.trim();
  c.auto_port = $("set-autoport").checked;
  c.autostart_server = $("set-autostart").checked;
  c.keep_awake = $("set-keepawake").checked;
  c.strict_host_check = $("set-hostcheck").checked;

  c.pin_enabled = $("set-pin-enabled").checked;
  c.pin = $("set-pin").value.trim();
  c.max_pin_attempts = parseIntOr($("set-attempts").value, 5);
  c.lockout_seconds = parseIntOr($("set-lockout").value, 30);
  c.session_ttl_hours = parseIntOr($("set-ttl").value, 12);

  c.uploads_enabled = $("set-uploads").checked;
  c.inbox_share_id = $("set-inbox").value || null;
  c.max_upload_mb = parseIntOr($("set-maxupload").value, 4096);

  c.thumbnails_enabled = $("set-thumbs").checked;
  c.allow_folder_zip = $("set-zip").checked;
  c.show_hidden = $("set-hidden").checked;
  c.thumb_max_edge = parseIntOr($("set-thumbsize").value, 320);
  c.default_view_mode = $("set-view").value;
  // null, not "", so the backend's Option round-trips and normalize can drop a
  // player that has since been uninstalled.
  c.external_player = $("set-player").value.trim() || null;

  c.theme = state.theme;
  return c;
}

const saveConfig = debounce(async function () {
  const next = readConfigFromUi();
  try {
    const result = await call("save_config", { config: next });
    state.config = next;
    if (result && result.warning) showToast(result.warning, "info");
    if (result && result.rebound) addLog("Server restarted on port " + result.port);
    await refreshServerStatus();
    await loadShares();
    // The backend normalizes (clamps ranges, enforces one inbox, backfills
    // tokens), so read it back rather than trusting the form values.
    await loadConfig();
  } catch (_err) {
    /* call() already logged and toasted */
  }
}, 350);

// ============================================================
// 6  Theme
// ============================================================

function applyTheme(theme) {
  const value = THEMES.includes(theme) ? theme : "dark";
  state.theme = value;
  document.documentElement.setAttribute("data-theme", value);
  document.querySelectorAll(".theme-card").forEach((card) => {
    card.classList.toggle("active", card.dataset.theme === value);
  });
}

function renderThemeGrid() {
  $("theme-grid").innerHTML = THEMES.map(
    (theme) => `
      <button class="theme-card ${theme === state.theme ? "active" : ""}" type="button" data-theme="${theme}">
        <span class="theme-card-check material-symbols-outlined">check</span>
        <span class="theme-preview theme-preview-${theme}">
          <span class="theme-preview-bar"></span>
          <span class="theme-preview-line theme-preview-line-full"></span>
          <span class="theme-preview-line theme-preview-line-2-3"></span>
        </span>
        <span class="theme-card-label">${THEME_LABELS[theme] || theme}</span>
      </button>`
  ).join("");
}

// ============================================================
// 7  Page routing
// ============================================================

/** What to refresh when a page becomes visible. Pages poll only while shown,
 *  so the Activity feed isn't hitting the backend from the Settings page. */
const PAGE_ENTER = {
  dashboard: () => {
    refreshServerStatus();
    renderDashboard();
  },
  shares: () => loadShares(),
  network: () => enterNetwork(),
  devices: () => {
    loadIdentity();
    startDevicesTick();
  },
  activity: () => {
    startActivityPoll();
    renderActivity();
  },
  settings: () => {
    loadSessions();
    loadThumbStats();
  },
};

function switchPage(page) {
  const target = PAGES.includes(page) ? page : "dashboard";
  state.currentPage = target;

  PAGES.forEach((p) => {
    $(p + "-view").classList.toggle("hidden", p !== target);
  });
  document.querySelectorAll("[data-sidebar-link]").forEach((link) => {
    link.classList.toggle("active", link.dataset.sidebarLink === target);
  });

  if (target !== "activity") stopActivityPoll();
  if (target !== "devices") stopDevicesTick();
  const enter = PAGE_ENTER[target];
  if (enter) enter();
}

// ============================================================
// 8  Server control
// ============================================================

async function refreshServerStatus() {
  const status = await callQuiet("get_server_status");
  if (!status) return;

  state.server.running = !!status.running;
  state.server.port = status.port;
  state.server.startedAtMs = status.started_at_ms;
  state.server.bytesServedText = status.bytes_served_text;
  state.server.sessionCount = status.session_count;
  state.server.shareCount = status.share_count;
  state.server.pin = status.pin;
  state.server.pinEnabled = status.pin_enabled;

  if (state.server.running) {
    const urls = await callQuiet("get_lan_urls");
    state.server.urls = Array.isArray(urls) ? urls : [];
    if (state.server.urlIndex >= state.server.urls.length) state.server.urlIndex = 0;
  } else {
    state.server.urls = [];
  }

  renderHeaderStatus();
  if (state.currentPage === "dashboard") renderDashboard();
}

function renderHeaderStatus() {
  const pill = $("header-status");
  const text = $("header-status-text");
  pill.className = "pill " + (state.server.running ? "pill-live" : "pill-off");
  text.textContent = state.server.running
    ? "Sharing on port " + state.server.port
    : "Stopped";
}

async function toggleServer() {
  if (state.server.transitioning) return;
  state.server.transitioning = true;
  renderPowerToggle();

  try {
    if (state.server.running) {
      await call("stop_server");
      addLog("Server stopped");
      showToast("Server stopped");
      state.server.lastError = null;
    } else {
      if (!state.shares.items.length) {
        showToast("Add a folder to share first", "error");
        return;
      }
      await call("start_server");
      state.server.lastError = null;
      addLog("Server started");
      showToast("Sharing started", "success");
      // The QR encodes the bound port, which auto-port may have changed.
      state.qrCache = {};
    }
  } catch (err) {
    // Port-in-use and firewall failures land here; the message names the fix,
    // so it stays on the Dashboard rather than only in a toast that vanishes.
    state.server.lastError = err.message;
  } finally {
    state.server.transitioning = false;
    await refreshServerStatus();
    await loadShares();
  }
}

function startServerPoll() {
  stopServerPoll();
  state.server.pollTimer = setInterval(refreshServerStatus, SERVER_POLL_MS);
}

function stopServerPoll() {
  if (state.server.pollTimer) clearInterval(state.server.pollTimer);
  state.server.pollTimer = null;
}

// ============================================================
// 9  Clipboard + QR
// ============================================================

async function copyText(text, label) {
  if (!text) return;
  try {
    // The Tauri webview is not always a secure context, and navigator.clipboard
    // is gated on that — hence the execCommand fallback.
    await navigator.clipboard.writeText(text);
    showToast((label || "Copied") + " copied", "success");
    return;
  } catch (_err) {
    /* fall through */
  }
  try {
    const area = document.createElement("textarea");
    area.value = text;
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    const ok = document.execCommand("copy");
    area.remove();
    showToast(ok ? (label || "Copied") + " copied" : "Could not copy", ok ? "success" : "error");
  } catch (_err2) {
    showToast("Could not copy", "error");
  }
}

async function renderQr() {
  const frame = $("qr-frame");
  const empty = $("qr-empty");

  if (!state.server.running || !state.server.urls.length) {
    frame.classList.add("hidden");
    empty.classList.remove("hidden");
    return;
  }

  const current = state.server.urls[state.server.urlIndex];
  if (!current) return;

  const key = current.ip + ":" + state.server.port;
  if (!state.qrCache[key]) {
    const payload = await callQuiet("get_share_qr", { shareId: null, ip: current.ip, size: 320 });
    if (!payload) return;
    state.qrCache[key] = payload.svg;
  }

  frame.innerHTML = state.qrCache[key];
  frame.classList.remove("hidden");
  empty.classList.add("hidden");
}

// ============================================================
// 10  Dashboard render
// ============================================================

function renderPowerToggle() {
  const toggle = $("power-toggle");
  toggle.classList.toggle("is-on", state.server.running);
  toggle.classList.toggle("is-busy", state.server.transitioning);
  toggle.disabled = state.server.transitioning;
  toggle.setAttribute("aria-pressed", String(state.server.running));
}

function renderDashboard() {
  renderPowerToggle();

  const hasShares = state.shares.items.length > 0;
  $("dashboard-empty").classList.toggle("hidden", hasShares);

  $("power-title").textContent = state.server.running ? "Sharing" : "Server stopped";
  $("power-sub").textContent = state.server.running
    ? "Anyone on this network can open the address below."
    : hasShares
      ? "Switch on to let other devices connect."
      : "Add a folder on the Shares page first.";

  // Errors
  const errorBox = $("server-error");
  if (state.server.lastError) {
    $("server-error-text").textContent = state.server.lastError;
    errorBox.classList.remove("hidden");
  } else {
    errorBox.classList.add("hidden");
  }

  // Address + interface picker
  const select = $("iface-select");
  const urls = state.server.urls;
  if (urls.length) {
    select.innerHTML = urls
      .map(
        (u, i) =>
          `<option value="${i}">${escapeHtml(u.label)} — ${escapeHtml(u.ip)}${u.is_primary ? " (main)" : ""}${u.is_virtual ? " (virtual)" : ""}</option>`
      )
      .join("");
    select.value = String(state.server.urlIndex);
    select.disabled = false;
    $("lan-url").textContent = urls[state.server.urlIndex].url;
    $("copy-url-btn").disabled = false;
    $("open-browser-btn").disabled = false;
  } else {
    // Offline, or every adapter filtered out.
    select.innerHTML = state.server.running
      ? '<option>No usable network found</option>'
      : '<option>Start the server to see addresses</option>';
    select.disabled = true;
    $("lan-url").textContent = state.server.running ? "No network address available" : "—";
    $("copy-url-btn").disabled = true;
    $("open-browser-btn").disabled = true;
  }

  // PIN
  const pinBlock = $("pin-block");
  pinBlock.classList.toggle("hidden", !state.server.pinEnabled);
  const pinValue = $("pin-value");
  if (state.pinRevealed) {
    pinValue.textContent = state.server.pin || "—";
    pinValue.classList.remove("is-masked");
  } else {
    pinValue.textContent = "••••••";
    pinValue.classList.add("is-masked");
  }

  // Counters
  $("stat-shares").textContent = state.shares.items.filter((s) => s.enabled).length;
  $("stat-bytes").textContent = state.server.bytesServedText || "0.0 B";
  $("stat-sessions").textContent = state.server.sessionCount || 0;
  $("stat-uptime").textContent = state.server.running
    ? formatUptime(Date.now() - state.server.startedAtMs)
    : "—";

  renderQr();
}

async function loadFirewallHint() {
  const hint = await callQuiet("get_firewall_hint");
  if (!hint) return;
  $("firewall-note").innerHTML =
    escapeHtml(hint.note) + "<code>" + escapeHtml(hint.command) + "</code>";
}

// ============================================================
// 11  Shares — data
// ============================================================

async function loadShares() {
  const items = await callQuiet("list_shares");
  state.shares.items = Array.isArray(items) ? items : [];
  renderSharesTable();
  renderInboxOptions();
  if (state.currentPage === "dashboard") renderDashboard();
}

async function addFolderShare() {
  const path = await callQuiet("pick_folder");
  if (!path) return;
  try {
    const view = await call("add_share", { path, name: null });
    addLog("Added share: " + view.name);
    showToast("Sharing " + view.name, "success");
    await loadShares();
    startShareIndex(view.id);
  } catch (_err) {
    /* already reported */
  }
}

async function addFileShares() {
  const paths = await callQuiet("pick_files");
  if (!paths || !paths.length) return;
  try {
    const views = await call("add_shares", { paths });
    addLog("Added " + formatCount(views.length, "file"));
    showToast("Added " + formatCount(views.length, "file"), "success");
    await loadShares();
  } catch (_err) {
    /* already reported */
  }
}

async function removeShare(shareId, name) {
  const ok = await confirmDialog(
    "Stop sharing " + name + "?",
    "The folder and its files stay exactly where they are — only the share is removed. Its secret link stops working.",
    "Stop sharing"
  );
  if (!ok) return;
  try {
    await call("remove_share", { shareId });
    addLog("Removed share: " + name);
    await loadShares();
  } catch (_err) {
    /* already reported */
  }
}

async function toggleShareEnabled(shareId, enabled) {
  try {
    await call("set_share_enabled", { shareId, enabled });
    await loadShares();
  } catch (_err) {
    /* already reported */
  }
}

async function regenerateToken(shareId, name) {
  const ok = await confirmDialog(
    "Make a new link for " + name + "?",
    "The old link stops working immediately, including for anyone browsing through it right now.",
    "New link"
  );
  if (!ok) return;
  try {
    await call("regenerate_share_token", { shareId });
    showToast("New link created", "success");
    addLog("Regenerated the link for " + name);
    await loadShares();
  } catch (_err) {
    /* already reported */
  }
}

function openShareEditor(shareId) {
  const share = state.shares.items.find((s) => s.id === shareId);
  if (!share) return;
  state.shares.editing = shareId;

  $("share-edit-title").textContent = "Edit " + share.name;
  $("share-edit-name").value = share.name;
  $("share-edit-note").value = share.note || "";
  $("share-edit-recursive").checked = !!share.recursive;
  $("share-edit-include").value = (share.include_ext || []).join(", ");
  $("share-edit-exclude").value = (share.exclude_ext || []).join(", ");
  $("share-edit-backdrop").classList.remove("hidden");
}

async function saveShareEdit() {
  const shareId = state.shares.editing;
  const original = state.shares.items.find((s) => s.id === shareId);
  if (!original) return;

  // Send back the whole Share record: update_share replaces it wholesale, and
  // it preserves the token itself so a rename can never rotate the secret.
  const share = {
    id: original.id,
    name: $("share-edit-name").value.trim() || original.name,
    path: original.path,
    token: original.token,
    enabled: original.enabled,
    is_inbox: original.is_inbox,
    read_only: original.read_only,
    is_file: original.is_file,
    recursive: $("share-edit-recursive").checked,
    include_ext: parseExtList($("share-edit-include").value),
    exclude_ext: parseExtList($("share-edit-exclude").value),
    added_ms: original.added_ms,
    note: $("share-edit-note").value.trim() || null,
  };

  try {
    await call("update_share", { share });
    $("share-edit-backdrop").classList.add("hidden");
    state.shares.editing = null;
    await loadShares();
    showToast("Share updated", "success");
  } catch (_err) {
    /* already reported */
  }
}

// ============================================================
// 12  Shares — render
// ============================================================

function renderSharesTable() {
  const tbody = $("shares-tbody");
  const needle = state.shares.filter.trim().toLowerCase();
  const rows = needle
    ? state.shares.items.filter(
        (s) =>
          s.name.toLowerCase().includes(needle) ||
          (s.display_path || "").toLowerCase().includes(needle)
      )
    : state.shares.items;

  $("shares-count").textContent = formatCount(state.shares.items.length, "share");

  if (!rows.length) {
    tbody.innerHTML = `
      <tr class="shares-empty-row">
        <td colspan="7">${
          state.shares.items.length
            ? "No shares match that filter."
            : "Nothing shared yet — use Add folder to pick one."
        }</td>
      </tr>`;
    return;
  }

  tbody.innerHTML = rows.map(shareRowHtml).join("");
}

function shareRowHtml(share) {
  const indexing = state.shares.indexing[share.id];
  const stats = state.shares.stats[share.id];

  let contents;
  if (indexing && indexing.running) {
    contents =
      escapeHtml(indexing.message || "Counting…") +
      '<div class="table-progress"><div class="table-progress-bar"></div></div>';
  } else if (stats) {
    contents = escapeHtml(formatCount(stats.file_count, "file") + " · " + stats.total_bytes_text);
  } else {
    contents = '<span class="text-dim">—</span>';
  }

  let status;
  if (!share.root_exists) {
    status = '<span class="share-status is-missing"><span class="share-status-dot"></span>Folder missing</span>';
  } else if (!share.enabled) {
    status = '<span class="share-status is-off"><span class="share-status-dot"></span>Paused</span>';
  } else if (state.server.running) {
    status = '<span class="share-status is-live"><span class="share-status-dot"></span>Live</span>';
  } else {
    status = '<span class="share-status is-off"><span class="share-status-dot"></span>Server off</span>';
  }

  const revealed = state.shares.revealed[share.id];
  const link = share.link;
  const linkCell = link
    ? `<div class="link-cell">
         <span class="link-text" data-tip="${escapeHtml(link)}">${escapeHtml(revealed ? link : maskLink(link))}</span>
         <button class="icon-button" type="button" data-act="reveal" data-id="${escapeHtml(share.id)}" aria-label="Show link"
                 data-tip="${revealed ? "Mask the link again." : "Show the whole link. It is hidden by default so it cannot be read over your shoulder."}">
           <span class="material-symbols-outlined">${revealed ? "visibility_off" : "visibility"}</span>
         </button>
         <button class="icon-button" type="button" data-act="copy-link" data-id="${escapeHtml(share.id)}" aria-label="Copy link"
                 data-tip="Copy this share's private link. Anyone who has it opens this folder — and only this folder — without the PIN.">
           <span class="material-symbols-outlined">content_copy</span>
         </button>
         <button class="icon-button" type="button" data-act="qr" data-id="${escapeHtml(share.id)}" aria-label="Show QR code"
                 data-tip="Show a QR code for this link, for a phone to scan.">
           <span class="material-symbols-outlined">qr_code_2</span>
         </button>
       </div>`
    : '<span class="text-dim" data-tip="Secret links only exist while the server is running.">Start the server</span>';

  return `
    <tr class="${share.enabled ? "" : "is-disabled"}">
      <td>
        <button class="switch ${share.enabled ? "is-on" : ""}" type="button"
                data-act="toggle" data-id="${escapeHtml(share.id)}"
                role="switch" aria-checked="${share.enabled}"
                aria-label="Share ${escapeHtml(share.name)}"
                data-tip="${share.enabled ? "Pause this share. It disappears from the visitor's list and its secret link stops working." : "Serve this share again."}"></button>
      </td>
      <td>
        <div class="sh-name">${escapeHtml(share.name)}
          ${share.is_inbox ? '<span class="badge badge-inbox">Inbox</span>' : ""}
          ${share.is_file ? '<span class="badge">File</span>' : ""}
        </div>
        ${share.note ? '<div class="text-dim" style="font-size:11.5px">' + escapeHtml(share.note) + "</div>" : ""}
      </td>
      <td><div class="sh-path" data-tip="${escapeHtml(share.display_path)}">${escapeHtml(share.display_path)}</div></td>
      <td class="sh-cell-num">${contents}</td>
      <td>${linkCell}</td>
      <td>${status}</td>
      <td class="sh-cell-actions">
        ${share.is_file && isPlayableFile(share.display_path)
          ? `<button class="icon-button" type="button" data-act="play" data-id="${escapeHtml(share.id)}" aria-label="Play"
                     data-tip="Play this here, in your own media player. Nothing is streamed — it is your file.">
               <span class="material-symbols-outlined">play_circle</span>
             </button>`
          : ""}
        <button class="icon-button" type="button" data-act="index" data-id="${escapeHtml(share.id)}" aria-label="Count files"
                data-tip="Count the files and add up the size in this folder. Reads the folder only — nothing is changed.">
          <span class="material-symbols-outlined">calculate</span>
        </button>
        <button class="icon-button" type="button" data-act="open" data-id="${escapeHtml(share.id)}" aria-label="Show in file manager"
                data-tip="Open this folder in File Explorer.">
          <span class="material-symbols-outlined">folder_open</span>
        </button>
        <button class="icon-button" type="button" data-act="edit" data-id="${escapeHtml(share.id)}" aria-label="Edit share"
                data-tip="Rename it, add a note, exclude subfolders, or limit which file types are visible.">
          <span class="material-symbols-outlined">tune</span>
        </button>
        <button class="icon-button" type="button" data-act="regen" data-id="${escapeHtml(share.id)}" aria-label="New secret link"
                data-tip="Issue a new secret link. The old one stops working at once, and anyone using it is signed out.">
          <span class="material-symbols-outlined">key</span>
        </button>
        <button class="icon-button" type="button" data-act="remove" data-id="${escapeHtml(share.id)}" aria-label="Stop sharing"
                data-tip="Stop sharing this folder. The folder and its files stay exactly where they are on disk.">
          <span class="material-symbols-outlined">delete</span>
        </button>
      </td>
    </tr>`;
}

/** Media a player is worth opening for. Deliberately wider than what a browser
 *  can decode — .mkv and .avi are the whole reason the Play button exists. */
const PLAYABLE_EXTS = [
  "mp4", "m4v", "mkv", "avi", "mov", "wmv", "webm", "flv", "mpg", "mpeg", "ts", "m2ts", "ogv",
  "3gp", "mp3", "m4a", "flac", "wav", "aac", "ogg", "oga", "opus", "wma", "aiff",
];

function isPlayableFile(name) {
  const idx = String(name || "").lastIndexOf(".");
  if (idx < 0) return false;
  return PLAYABLE_EXTS.includes(name.slice(idx + 1).toLowerCase());
}

/** Show enough of the link to recognise it, not enough to use it over a
 *  shoulder or in a screenshot. */
function maskLink(link) {
  const idx = link.lastIndexOf("/s/");
  if (idx < 0) return link;
  return link.slice(0, idx + 3) + "••••••••";
}

async function onSharesTableClick(event) {
  const button = event.target.closest("[data-act]");
  if (!button) return;
  const id = button.dataset.id;
  const share = state.shares.items.find((s) => s.id === id);
  if (!share) return;

  switch (button.dataset.act) {
    case "toggle":
      return toggleShareEnabled(id, !share.enabled);
    case "reveal":
      state.shares.revealed[id] = !state.shares.revealed[id];
      return renderSharesTable();
    case "copy-link":
      return copyText(share.link, "Link");
    case "qr":
      return showShareQr(share);
    case "index":
      return startShareIndex(id);
    case "open":
      return callQuiet("show_in_explorer", { path: share.display_path });
    case "play":
      try {
        await call("open_in_player", { path: share.display_path });
      } catch (_err) {
        /* already reported */
      }
      return;
    case "edit":
      return openShareEditor(id);
    case "regen":
      return regenerateToken(id, share.name);
    case "remove":
      return removeShare(id, share.name);
  }
}

async function showShareQr(share) {
  const payload = await callQuiet("get_share_qr", { shareId: share.id, ip: null, size: 320 });
  if (!payload) {
    showToast("Start the server first", "error");
    return;
  }
  $("confirm-title").textContent = share.name;
  $("confirm-body").innerHTML =
    '<div class="qr-frame" style="margin:12px auto">' + payload.svg + "</div>" +
    '<div class="mono text-dim" style="font-size:11.5px;word-break:break-all;text-align:center">' +
    escapeHtml(payload.url) + "</div>";
  $("confirm-ok").textContent = "Copy link";
  $("confirm-backdrop").classList.remove("hidden");
  state.pendingConfirm = (ok) => {
    // The body was replaced with markup; put the plain paragraph back so the
    // next confirm dialog doesn't inherit a QR code.
    $("confirm-body").innerHTML = "";
    if (ok) copyText(payload.url, "Link");
  };
}

function renderInboxOptions() {
  const select = $("set-inbox");
  if (!select) return;
  const current = state.config ? state.config.inbox_share_id : null;
  const folders = state.shares.items.filter((s) => !s.is_file);
  select.innerHTML =
    '<option value="">None</option>' +
    folders
      .map((s) => `<option value="${escapeHtml(s.id)}">${escapeHtml(s.name)}</option>`)
      .join("");
  select.value = current || "";
}

// ============================================================
// 13  Shares — index task
// ============================================================

async function startShareIndex(shareId) {
  if (state.shares.indexing[shareId] && state.shares.indexing[shareId].running) return;
  state.shares.indexing[shareId] = { running: true, message: "Counting…" };
  renderSharesTable();

  try {
    const payload = await runTask("start_index_share_task", { shareId }, (p) => {
      const entry = state.shares.indexing[shareId];
      if (entry) {
        entry.message = p.message || "Counting…";
        renderSharesTable();
      }
    });
    const result = payload.index_result;
    if (result) {
      state.shares.stats[shareId] = result;
      if (result.skipped) {
        addLog(
          "Indexed " + shareId + " — " + result.skipped + " unreadable folder(s) skipped"
        );
      }
    }
  } catch (err) {
    addLog("Could not count files: " + err.message);
  } finally {
    delete state.shares.indexing[shareId];
    renderSharesTable();
  }
}

// ============================================================
// 14  Preview page
// ============================================================
//
// An iframe against the real receiver page on loopback, not a rebuilt gallery.
// You see exactly what receivers see — including anything that leaks — and it
// exercises the genuine token-auth path, with no second UI to keep in sync.

// ============================================================
// 15  Activity page
// ============================================================

const ACTION_LABELS = {
  auth: "sign in",
  auth_failed: "bad PIN",
  list: "browse",
  view: "stream",
  download: "download",
  upload: "upload",
  thumb: "thumbnail",
  // A link handed to an external player. Logged where it is issued, which is
  // before — and sometimes instead of — any bytes moving.
  play: "open in player",
  denied: "denied",
};

function startActivityPoll() {
  stopActivityPoll();
  pollActivity();
  state.activity.pollTimer = setInterval(() => {
    if (state.activity.autoRefresh) pollActivity();
  }, ACTIVITY_POLL_MS);
}

function stopActivityPoll() {
  if (state.activity.pollTimer) clearInterval(state.activity.pollTimer);
  state.activity.pollTimer = null;
}

async function pollActivity() {
  // sinceId means the payload is normally empty; a full refetch every 1.5s
  // would re-serialise 2,000 rows for nothing.
  const fresh = await callQuiet("get_activity_log", {
    sinceId: state.activity.lastId,
    limit: ACTIVITY_LIMIT,
  });
  if (!Array.isArray(fresh)) return;

  if (fresh.length) {
    state.activity.lastId = Math.max(state.activity.lastId, fresh[0].id);
    state.activity.entries = fresh.concat(state.activity.entries).slice(0, 2000);
    renderActivity();
  } else {
    // In-flight transfers mutate rows we already hold (bytes + status), so a
    // periodic full refresh keeps their progress honest.
    const all = await callQuiet("get_activity_log", { sinceId: 0, limit: ACTIVITY_LIMIT });
    if (Array.isArray(all) && all.length) {
      state.activity.entries = all;
      state.activity.lastId = all[0].id;
      renderActivity();
    }
  }
}

function renderActivity() {
  const tbody = $("activity-tbody");
  const filter = state.activity.filter;

  const rows = state.activity.entries.filter((e) => {
    if (filter === "all") return true;
    if (filter === "download")
      return (
        e.kind === "download" || e.kind === "view" || e.kind === "upload" || e.kind === "play"
      );
    if (filter === "auth") return e.kind === "auth" || e.kind === "auth_failed";
    if (filter === "denied") return e.kind === "denied" || e.kind === "auth_failed";
    return true;
  });

  $("activity-count").textContent = formatCount(state.activity.entries.length, "event");

  if (!rows.length) {
    tbody.innerHTML = `
      <tr class="activity-empty-row">
        <td colspan="6">${
          state.activity.entries.length ? "Nothing matches that filter." : "No activity yet."
        }</td>
      </tr>`;
    return;
  }

  tbody.innerHTML = rows
    .slice(0, 400)
    .map((entry) => {
      const bytes =
        entry.total_bytes && entry.status !== "ok"
          ? formatBytes(entry.bytes) + " / " + formatBytes(entry.total_bytes)
          : entry.bytes
            ? formatBytes(entry.bytes)
            : "";
      return `
      <tr>
        <td class="act-time">${escapeHtml(formatClock(entry.at_ms))}</td>
        <td class="act-ip">${escapeHtml(entry.client_ip)}</td>
        <td><span class="act-action act-${escapeHtml(entry.kind)}">${escapeHtml(ACTION_LABELS[entry.kind] || entry.kind)}</span></td>
        <td>
          <div class="act-cell-file"${entry.path ? ` data-tip="${escapeHtml(entry.path)}"` : ""}>${escapeHtml(entry.path || "—")}</div>
          ${entry.share_name ? '<div class="act-share">' + escapeHtml(entry.share_name) + "</div>" : ""}
        </td>
        <td class="act-bytes">${escapeHtml(bytes)}</td>
        <td><span class="act-status act-status-${escapeHtml(entry.status)}">${escapeHtml(entry.status)}</span>
          ${entry.detail ? '<div class="act-share">' + escapeHtml(entry.detail) + "</div>" : ""}
        </td>
      </tr>`;
    })
    .join("");
}

// ============================================================
// 16  Settings page
// ============================================================

async function loadSessions() {
  const sessions = await callQuiet("list_sessions");
  const host = $("sessions-list");
  if (!Array.isArray(sessions) || !sessions.length) {
    host.innerHTML = '<div class="text-dim" style="font-size:12.5px">No devices connected.</div>';
    return;
  }
  host.innerHTML = sessions
    .map(
      (s) => `
      <div class="session-row">
        <span class="mono">${escapeHtml(s.client_ip)}</span>
        <span class="badge">${escapeHtml(s.scope)}</span>
        <span class="spacer"><span class="session-ua">${escapeHtml(s.user_agent || "unknown device")}</span></span>
        <span class="text-dim">${escapeHtml(formatClock(s.last_seen_ms))}</span>
        <button class="icon-button" type="button" data-session="${escapeHtml(s.token)}" aria-label="Sign this device out"
                data-tip="Sign this one browser out. It needs the PIN again; every other device is unaffected.">
          <span class="material-symbols-outlined">logout</span>
        </button>
      </div>`
    )
    .join("");
}

async function loadThumbStats() {
  const stats = await callQuiet("get_thumb_cache_stats");
  $("thumb-cache-size").textContent = stats
    ? stats.total_bytes_text + " · " + formatCount(stats.file_count, "file")
    : "—";
}

// ============================================================
// 17  Event wiring + boot
// ============================================================

function wire() {
  // Delegated from `document`, so it also covers every row rendered later.
  wireTooltips();

  // --- sidebar ---
  document.querySelectorAll("[data-sidebar-link]").forEach((link) => {
    link.addEventListener("click", (event) => {
      event.preventDefault();
      switchPage(link.dataset.sidebarLink);
    });
  });

  $("sidebar-toggle").addEventListener("click", () => {
    const collapsed = document.body.classList.toggle("sidebar-collapsed");
    $("sidebar-toggle-icon").textContent = collapsed ? "menu" : "menu_open";
  });

  // --- header ---
  $("theme-toggle").addEventListener("click", () => switchPage("settings"));
  $("console-toggle").addEventListener("click", () =>
    $("console-drawer").classList.toggle("hidden")
  );
  $("console-close").addEventListener("click", () => $("console-drawer").classList.add("hidden"));
  $("console-clear").addEventListener("click", () => {
    state.logs = [];
    $("console-body").innerHTML = "";
  });

  // --- dashboard ---
  $("power-toggle").addEventListener("click", toggleServer);
  $("copy-url-btn").addEventListener("click", () => {
    const current = state.server.urls[state.server.urlIndex];
    if (current) copyText(current.url, "Address");
  });
  $("open-browser-btn").addEventListener("click", () => {
    const current = state.server.urls[state.server.urlIndex];
    if (current) callQuiet("open_url", { url: current.url });
  });
  $("iface-select").addEventListener("change", (event) => {
    state.server.urlIndex = parseIntOr(event.target.value, 0);
    renderDashboard();
  });
  $("pin-reveal-btn").addEventListener("click", () => {
    state.pinRevealed = !state.pinRevealed;
    renderDashboard();
  });
  $("pin-regen-btn").addEventListener("click", async () => {
    const ok = await confirmDialog(
      "Generate a new PIN?",
      "Devices already connected stay connected. Anyone joining from now on needs the new PIN.",
      "New PIN"
    );
    if (!ok) return;
    try {
      const pin = await call("generate_pin");
      state.pinRevealed = true;
      showToast("New PIN: " + pin, "success");
      await loadConfig();
      await refreshServerStatus();
    } catch (_err) {
      /* already reported */
    }
  });

  // --- shares ---
  $("add-folder-btn").addEventListener("click", addFolderShare);
  $("add-files-btn").addEventListener("click", addFileShares);
  $("shares-tbody").addEventListener("click", onSharesTableClick);
  $("shares-filter").addEventListener(
    "input",
    debounce((event) => {
      state.shares.filter = event.target.value;
      renderSharesTable();
    }, 140)
  );

  $("share-edit-save").addEventListener("click", saveShareEdit);
  $("share-edit-cancel").addEventListener("click", () => {
    $("share-edit-backdrop").classList.add("hidden");
    state.shares.editing = null;
  });

  // --- activity ---
  $("activity-filter").addEventListener("click", (event) => {
    const button = event.target.closest("[data-filter]");
    if (!button) return;
    state.activity.filter = button.dataset.filter;
    $("activity-filter")
      .querySelectorAll(".mode-button")
      .forEach((b) => b.classList.toggle("active", b === button));
    renderActivity();
  });
  $("activity-auto").addEventListener("change", (event) => {
    state.activity.autoRefresh = event.target.checked;
  });
  $("activity-clear").addEventListener("click", async () => {
    await callQuiet("clear_activity_log");
    state.activity.entries = [];
    state.activity.lastId = 0;
    renderActivity();
  });

  // --- settings ---
  [
    "set-port", "set-bind", "set-name", "set-autoport", "set-autostart",
    "set-keepawake", "set-hostcheck", "set-pin-enabled", "set-pin",
    "set-attempts", "set-lockout", "set-ttl", "set-uploads", "set-inbox",
    "set-maxupload", "set-thumbs", "set-zip", "set-hidden", "set-thumbsize",
    "set-view", "set-player",
  ].forEach((id) => {
    const el = $(id);
    if (!el) return;
    el.addEventListener(el.type === "checkbox" || el.tagName === "SELECT" ? "change" : "input", saveConfig);
  });

  $("set-pin-regen").addEventListener("click", async () => {
    try {
      const pin = await call("generate_pin");
      $("set-pin").value = pin;
      await loadConfig();
    } catch (_err) {
      /* already reported */
    }
  });

  $("clear-thumbs-btn").addEventListener("click", async () => {
    const freed = await callQuiet("clear_thumb_cache");
    showToast("Freed " + formatBytes(freed || 0), "success");
    loadThumbStats();
  });

  $("set-player-detect").addEventListener("click", async () => {
    const found = await callQuiet("detect_player");
    if (!found) {
      showToast("No player found in the usual places — paste the path instead", "info");
      return;
    }
    $("set-player").value = found;
    saveConfig();
    showToast("Using " + found, "success");
  });

  $("revoke-all-btn").addEventListener("click", async () => {
    const ok = await confirmDialog(
      "Sign out every device?",
      "Everyone currently browsing will have to enter the PIN again.",
      "Sign all out"
    );
    if (!ok) return;
    const count = await callQuiet("revoke_all_sessions");
    showToast(formatCount(count || 0, "device") + " signed out", "success");
    loadSessions();
  });

  $("sessions-list").addEventListener("click", async (event) => {
    const button = event.target.closest("[data-session]");
    if (!button) return;
    await callQuiet("revoke_session", { token: button.dataset.session });
    loadSessions();
  });

  $("theme-grid").addEventListener("click", (event) => {
    const card = event.target.closest("[data-theme]");
    if (!card) return;
    applyTheme(card.dataset.theme);
    saveConfig();
  });

  // --- dialogs ---
  $("confirm-ok").addEventListener("click", () => closeConfirm(true));
  $("confirm-cancel").addEventListener("click", () => closeConfirm(false));
  $("confirm-backdrop").addEventListener("click", (event) => {
    if (event.target === $("confirm-backdrop")) closeConfirm(false);
  });
  $("share-edit-backdrop").addEventListener("click", (event) => {
    if (event.target === $("share-edit-backdrop")) {
      $("share-edit-backdrop").classList.add("hidden");
      state.shares.editing = null;
    }
  });

  wireDevices();
  wireNetwork();

  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    if (!$("confirm-backdrop").classList.contains("hidden")) closeConfirm(false);
    if (!$("share-edit-backdrop").classList.contains("hidden")) {
      $("share-edit-backdrop").classList.add("hidden");
      state.shares.editing = null;
    }
  });
}

async function boot() {
  if (!invoke) {
    document.body.innerHTML =
      '<p style="padding:40px;font-family:sans-serif">This page has to run inside the LAN Share app.</p>';
    return;
  }

  renderThemeGrid();
  wire();

  await loadConfig();
  await loadShares();
  await refreshServerStatus();
  await loadFirewallHint();

  await loadIdentity();

  switchPage("dashboard");
  startServerPoll();
  // Pair requests and file offers must be noticed from any page, so this
  // one never stops.
  startWatchTick();

  state.booted = true;
  addLog("LAN Share ready");
}

document.addEventListener("DOMContentLoaded", boot);

// ============================================================
// 18  Devices — discovery, pairing, transfers
// ============================================================

const DEVICES_TICK_MS = 1500;
/// Incoming pair requests and file offers must be noticed from ANY page, so
/// they poll on their own slower timer that never stops. Without this, a
/// request that arrives while you are in Settings is simply never seen.
const WATCH_TICK_MS = 2000;
/// The Accept button on an incoming pair request stays disabled this long. A
/// modal that appears on its own, under a cursor already in motion, gets
/// clicked — and clicking it through is exactly the failure the code
/// comparison exists to prevent.
const PAIR_ACCEPT_ARM_MS = 1500;

state.devices = {
  self: null,
  // `running` means we are announcing (they can see us); `listening` means the
  // socket is up (we can see them). Going invisible only stops the first.
  discovery: { running: false, listening: false, health: "ok", error: null, devices: [] },
  peers: [],
  filter: "",
  tickTimer: null,
};

state.pairing = {
  // Outgoing: the task we are polling, and the last payload seen.
  taskId: null,
  cancelled: false,
  // Incoming: the prompt currently on screen, so a re-poll does not reopen it.
  shownPairId: null,
  acceptArmedAt: 0,
};


// --- helpers ---------------------------------------------------------------

const PLATFORM_ICON = {
  windows: "desktop_windows",
  macos: "laptop_mac",
  linux: "terminal",
};

function deviceIcon(platform) {
  return PLATFORM_ICON[platform] || "devices";
}

function relativeTime(ms) {
  if (!ms) return "never";
  const delta = Date.now() - Number(ms);
  if (delta < 60_000) return "just now";
  if (delta < 3_600_000) return Math.floor(delta / 60_000) + "m ago";
  if (delta < 86_400_000) return Math.floor(delta / 3_600_000) + "h ago";
  return Math.floor(delta / 86_400_000) + "d ago";
}

function formatRate(bytesPerSecond) {
  if (!bytesPerSecond || bytesPerSecond < 1) return "";
  return formatBytes(bytesPerSecond) + "/s";
}

function formatEta(remaining, bytesPerSecond) {
  if (!bytesPerSecond || bytesPerSecond < 1 || remaining <= 0) return "";
  const seconds = Math.round(remaining / bytesPerSecond);
  if (seconds < 60) return seconds + "s left";
  if (seconds < 3600) return Math.round(seconds / 60) + "m left";
  return Math.round(seconds / 3600) + "h left";
}

/// Render six digits as two groups of three. The eye compares two short chunks
/// reliably; six in a row it does not. Anything that is not exactly six digits
/// is refused rather than rendered, because a malformed code must never be
/// presented as something to compare.
function pairCodeHtml(code) {
  const digits = String(code || "");
  if (!/^\d{6}$/.test(digits)) {
    return '<span class="pair-code-group">------</span>';
  }
  return (
    '<span class="pair-code-group">' + digits.slice(0, 3) + "</span>" +
    '<span class="pair-code-sep"></span>' +
    '<span class="pair-code-group">' + digits.slice(3) + "</span>"
  );
}

// --- polling ---------------------------------------------------------------

function startDevicesTick() {
  stopDevicesTick();
  refreshDevices();
  state.devices.tickTimer = setInterval(refreshDevices, DEVICES_TICK_MS);
}

function stopDevicesTick() {
  if (state.devices.tickTimer) clearInterval(state.devices.tickTimer);
  state.devices.tickTimer = null;
}

/// One tick for the whole page. Four independent intervals hammering the
/// backend would be four times the work for the same information.
async function refreshDevices() {
  const [discovery, peers] = await Promise.all([
    callQuiet("list_discovered"),
    callQuiet("list_peers"),
  ]);
  if (discovery) state.devices.discovery = discovery;
  if (Array.isArray(peers)) state.devices.peers = peers;

  renderDiscovered();
  renderPeers();
  renderDiscoveryNotice();
  renderPresence();
}

/// Runs on every page, forever. A pairing request is another human waiting on
/// an answer; missing one because you were on Settings is the failure mode.
function startWatchTick() {
  setInterval(async () => {
    const requests = await callQuiet("list_incoming_pair_requests");
    if (Array.isArray(requests)) showIncomingPair(requests);
    updateDevicesBadge(Array.isArray(requests) ? requests.length : 0);
  }, WATCH_TICK_MS);
}

function updateDevicesBadge(count) {
  const badge = $("devices-badge");
  badge.textContent = count;
  badge.classList.toggle("hidden", count === 0);
}

// --- this device -----------------------------------------------------------

async function loadIdentity() {
  const identity = await callQuiet("get_device_identity");
  if (!identity) return;
  state.devices.self = identity;

  // Do not stomp on the field while it is being edited.
  if (document.activeElement !== $("self-name")) $("self-name").value = identity.name;
  $("self-addr").textContent = identity.addresses.length
    ? identity.addresses[0]
    : "no network";
  renderPresence();
}

/// Whether other devices can see this one. Not a control any more -- running
/// the server IS the consent, so this only reports.
function renderPresence() {
  const { running, error } = state.devices.discovery;
  const pill = $("visibility-pill");
  pill.classList.toggle("pill-live", running);
  pill.classList.toggle("pill-warn", !running && !!error);
  pill.classList.toggle("pill-off", !running && !error);
  $("visibility-pill-text").textContent = running
    ? "Visible"
    : error
      ? "Discovery failed"
      : state.server.running
        ? "Not announcing"
        : "Server stopped";
}

// --- discovered ------------------------------------------------------------

/// One row per device, even when it announces from two interfaces.
function dedupeDiscovered(list) {
  const byId = new Map();
  for (const item of list) {
    const existing = byId.get(item.device_id);
    if (!existing) {
      byId.set(item.device_id, { ...item, addresses: [...(item.addresses || [])] });
      continue;
    }
    existing.online = existing.online || item.online;
    existing.last_seen_ms = Math.max(existing.last_seen_ms, item.last_seen_ms);
    for (const address of item.addresses || []) {
      if (!existing.addresses.includes(address)) existing.addresses.push(address);
    }
  }
  return [...byId.values()];
}

function renderDiscovered() {
  const host = $("nearby-list");
  const radar = $("radar");
  // Scanning is about the ear, not the mouth: a hidden device still hears
  // every beacon on the network, so the radar keeps sweeping while it does.
  const listening = state.devices.discovery.listening;
  radar.classList.toggle("is-scanning", listening);

  // Already-paired devices belong in the list below. Showing them in both
  // invites re-pairing something already trusted.
  const rows = dedupeDiscovered(state.devices.discovery.devices || []).filter(
    (d) => !d.paired
  );
  $("nearby-count").textContent = rows.length;

  if (!rows.length) {
    // Three states, not two. Being hidden does not stop the search, so saying
    // "discovery is off" while we are still listening would send the user off
    // to flip a switch that is not the problem.
    let title = "Looking for devices…";
    let body = "Open LAN Share on another computer on this Wi-Fi and it will appear here.";
    if (!listening && !state.server.running) {
      title = "Discovery is off";
      body = "Start the server on the Dashboard to look for devices.";
    } else if (!listening) {
      title = "Discovery is off";
      body = "The discovery socket isn’t running. You can still add a device by address.";
    }
    host.innerHTML = `
      <div class="device-empty">
        <span class="material-symbols-outlined">${listening ? "wifi_tethering" : "wifi_off"}</span>
        <span class="device-empty-title">${title}</span>
        <span class="device-empty-body">${body}</span>
      </div>`;
    return;
  }

  host.innerHTML = rows
    .map((d) => {
      const extra = d.addresses.length > 1
        ? `<span class="badge" data-tip="This device answers on more than one address: ${escapeHtml(d.addresses.join(", "))}. Usually Wi-Fi and Ethernet at once.">+${d.addresses.length - 1} more</span>`
        : "";
      return `
      <div class="device-row ${d.online ? "is-online" : ""}">
        <span class="device-icon"><span class="material-symbols-outlined">${deviceIcon(d.platform)}</span></span>
        <span class="device-body">
          <span class="device-name">${escapeHtml(d.name || d.device_id)}</span>
          <span class="device-sub">
            <span class="share-status ${d.online ? "is-live" : "is-off"}">
              <span class="share-status-dot"></span>${d.online ? "Online" : relativeTime(d.last_seen_ms)}
            </span>
            <span class="mono">${escapeHtml(d.addresses[0] || "")}</span>
            ${extra}
            ${d.manual ? '<span class="badge" data-tip="You typed this one in rather than hearing it. It is never dropped from the list automatically.">Added by hand</span>' : ""}
          </span>
        </span>
        <span class="device-auto is-hidden-soft"></span>
        <!-- The tip sits on the wrapper while the button is disabled: a
             disabled button is inert and never sees the pointer, so a tip on
             it would be silent in exactly the case that needs explaining. -->
        <span class="device-actions"${d.online ? "" : ' data-tip="This device has gone quiet, so there is nothing to pair with. It comes back on its own within seconds of reappearing."'}>
          <button class="primary-button" type="button" data-dev="pair" data-id="${escapeHtml(d.device_id)}" ${d.online ? "" : "disabled"}
                  data-tip="Ask this device to pair. Both screens then show the same six digits — check they match, then accept on the other one.">
            <span class="material-symbols-outlined">link</span>
            <span>Pair</span>
          </button>
        </span>
      </div>`;
    })
    .join("");
}

function renderDiscoveryNotice() {
  const notice = $("discovery-notice");
  const { health, error, running } = state.devices.discovery;

  if (!running && !error) {
    notice.classList.add("hidden");
    return;
  }
  let title = "";
  let body = "";
  if (error) {
    title = "Discovery could not start";
    body = escapeHtml(error) + " — you can still add devices by address.";
  } else if (health === "inbound_likely_blocked") {
    title = "Your firewall may be blocking discovery";
    body =
      "Windows asks separately for UDP, and that prompt may have been dismissed. " +
      "Other devices probably cannot see this one. You can still add them by address.";
  } else if (health === "nothing_heard") {
    title = "No other devices found";
    body =
      "This device is announcing itself correctly. Check LAN Share is running on the other " +
      "computer and both are on the same Wi-Fi. Some routers stop devices from seeing each " +
      "other at all — if so, add the device by address.";
  } else {
    notice.classList.add("hidden");
    return;
  }
  $("discovery-notice-title").textContent = title;
  $("discovery-notice-body").innerHTML = body;
  notice.classList.remove("hidden");
}

// --- paired ----------------------------------------------------------------

function renderPeers() {
  const host = $("paired-list");
  const rows = state.devices.peers;
  $("paired-count").textContent = rows.length;

  if (!rows.length) {
    host.innerHTML = `
      <div class="device-empty">
        <span class="material-symbols-outlined">devices_other</span>
        <span class="device-empty-title">No paired devices yet</span>
        <span class="device-empty-body">Pair with a device above. You only do this once per
          device — after that you can send files straight to it.</span>
      </div>`;
    return;
  }

  host.innerHTML = rows.map(peerRowHtml).join("");
}

function peerRowHtml(p) {
  const id = escapeHtml(p.device_id);
  const status = p.blocked
    ? '<span class="share-status is-blocked"><span class="share-status-dot"></span>Blocked</span>'
    : p.online
      ? '<span class="share-status is-live"><span class="share-status-dot"></span>Online</span>'
      : `<span class="share-status is-off"><span class="share-status-dot"></span>${relativeTime(p.last_seen_ms)}</span>`;

  // A blocked device keeps its row and its unblock button. Hiding it would
  // make the only route to undoing a block "remember that you did it".
  const actions = p.blocked
    ? `<button class="ghost-button" type="button" data-dev="unblock" data-id="${id}"
               data-tip="Let this device talk to you again. It stays paired throughout — blocking never lost the pairing.">Unblock</button>
       <button class="icon-button" type="button" data-dev="unpair" data-id="${id}" aria-label="Unpair"
               data-tip="Forget this device entirely. You would both have to pair again from scratch.">
         <span class="material-symbols-outlined">link_off</span>
       </button>`
    : `<button class="primary-button" type="button" data-dev="browse" data-id="${id}" ${p.online ? "" : "disabled"}
               data-tip="Open this device on the Network page: browse what it shares, preview it, and pull down what you want.">
         <span class="material-symbols-outlined">folder_open</span><span>Browse</span>
       </button>
       <button class="icon-button" type="button" data-dev="rename" data-id="${id}" aria-label="Rename"
               data-tip="Rename this device in your list. Only you see the new name.">
         <span class="material-symbols-outlined">edit</span>
       </button>
       <button class="icon-button" type="button" data-dev="block" data-id="${id}" aria-label="Block"
               data-tip="Refuse everything from this device. It stays in the list, so you can undo it later.">
         <span class="material-symbols-outlined">block</span>
       </button>
       <button class="icon-button" type="button" data-dev="unpair" data-id="${id}" aria-label="Unpair"
               data-tip="Forget this device. Nothing is deleted, but you would both have to pair again.">
         <span class="material-symbols-outlined">link_off</span>
       </button>`;

  return `
    <div class="device-row ${p.online ? "is-online" : ""} ${p.blocked ? "is-blocked" : ""}">
      <span class="device-icon"><span class="material-symbols-outlined">${deviceIcon(p.platform)}</span></span>
      <span class="device-body">
        <span class="device-name">${escapeHtml(p.name)}</span>
        <span class="device-sub">
          ${status}
          <span class="mono">${escapeHtml(p.address || "")}</span>
        </span>
      </span>
      <span class="device-auto ${p.blocked ? "is-hidden-soft" : ""}">
        <span class="device-auto-label">Always accept</span>
        <button class="switch ${p.auto_accept ? "is-on" : ""}" type="button"
                data-dev="auto" data-id="${id}" role="switch"
                aria-checked="${p.auto_accept}" aria-label="Always accept files from ${escapeHtml(p.name)}"
                data-tip="${p.auto_accept ? "Go back to asking before files from this device are saved." : "Save files from this device without prompting. Only do this for a device you own."}"></button>
      </span>
      <!-- Wrapper tip, because a disabled Send/Browse button never sees the
           pointer and so could not explain its own greyed-out state. -->
      <span class="device-actions"${p.online || p.blocked ? "" : ' data-tip="Offline: Send and Browse need the other device awake and running LAN Share. Everything else here still works."'}>${actions}</span>
    </div>`;
}

// --- transfers -------------------------------------------------------------

// --- outgoing pairing ------------------------------------------------------

async function startPairing(deviceId) {
  const device = (state.devices.discovery.devices || []).find((d) => d.device_id === deviceId);
  const name = device ? device.name : "device";

  $("pair-out-name").textContent = name;
  $("pair-out-code").innerHTML = pairCodeHtml("");
  $("pair-out-message").textContent = "Contacting the device…";
  $("pair-out-message").classList.remove("is-bad");
  $("pair-out-backdrop").classList.remove("hidden");
  state.pairing.cancelled = false;

  let handle;
  try {
    handle = await call("start_pair_task", { deviceId });
  } catch (_err) {
    $("pair-out-backdrop").classList.add("hidden");
    return;
  }
  state.pairing.taskId = handle.id;

  try {
    while (true) {
      await sleep(TASK_POLL_MS);
      const payload = await callQuiet("get_task_progress", { taskId: handle.id });
      if (!payload) throw new Error("lost contact with the pairing");

      const result = payload.pair_result;
      if (result && result.code) {
        $("pair-out-code").innerHTML = pairCodeHtml(result.code);
        $("pair-out-message").textContent =
          "Waiting for " + (result.peer_name || name) + " to accept…";
      }
      if (payload.error) throw new Error(payload.error);
      if (payload.done) {
        finishPairing(result, name);
        return;
      }
    }
  } catch (err) {
    $("pair-out-message").textContent = String(err.message || err);
    $("pair-out-message").classList.add("is-bad");
  } finally {
    await callQuiet("clear_task", { taskId: handle.id });
    state.pairing.taskId = null;
  }
}

function finishPairing(result, fallbackName) {
  const status = result ? result.status : "error";
  if (status === "accepted") {
    $("pair-out-backdrop").classList.add("hidden");
    showToast("Paired with " + (result.peer_name || fallbackName), "success");
    addLog("Paired with " + (result.peer_name || fallbackName));
    refreshDevices();
    return;
  }
  const message =
    status === "declined"
      ? (result.peer_name || fallbackName) + " declined"
      : status === "expired"
        ? "No answer from the other device"
        : status === "cancelled"
          ? "Cancelled"
          : result && result.message
            ? result.message
            : "Pairing failed";
  $("pair-out-message").textContent = message;
  $("pair-out-message").classList.add("is-bad");
}

async function cancelPairing() {
  state.pairing.cancelled = true;
  if (state.pairing.taskId != null) {
    await callQuiet("cancel_task", { taskId: state.pairing.taskId });
  }
  $("pair-out-backdrop").classList.add("hidden");
}

// --- incoming pairing ------------------------------------------------------

function showIncomingPair(requests) {
  const backdrop = $("pair-in-backdrop");
  if (!requests.length) {
    // Withdrawn or expired while on screen.
    if (state.pairing.shownPairId) {
      backdrop.classList.add("hidden");
      state.pairing.shownPairId = null;
    }
    return;
  }
  const request = requests[0];
  if (state.pairing.shownPairId === request.pair_id) return;

  state.pairing.shownPairId = request.pair_id;
  $("pair-in-name").textContent = request.name || request.device_id;
  $("pair-in-addr").textContent = request.address;
  $("pair-in-code").innerHTML = pairCodeHtml(request.code);

  // Disabled briefly and never focused. This is the load-bearing part: a
  // dialog that appears under a moving cursor gets clicked.
  const accept = $("pair-in-accept");
  accept.disabled = true;
  state.pairing.acceptArmedAt = Date.now() + PAIR_ACCEPT_ARM_MS;
  setTimeout(() => {
    if (state.pairing.shownPairId === request.pair_id) accept.disabled = false;
  }, PAIR_ACCEPT_ARM_MS);

  backdrop.classList.remove("hidden");
  $("pair-in-reject").focus();
}

async function answerIncomingPair(accept) {
  const pairId = state.pairing.shownPairId;
  if (!pairId) return;
  $("pair-in-backdrop").classList.add("hidden");
  state.pairing.shownPairId = null;
  try {
    if (accept) {
      const name = await call("accept_pair_request", { pairId });
      showToast("Paired with " + name, "success");
      addLog("Paired with " + name);
    } else {
      await call("decline_pair_request", { pairId });
    }
  } catch (_err) {
    /* already reported */
  }
  refreshDevices();
}

// --- incoming offers -------------------------------------------------------

// --- sending ---------------------------------------------------------------

// --- browse ----------------------------------------------------------------

// --- delegated actions -----------------------------------------------------

async function onDeviceAction(event) {
  const button = event.target.closest("[data-dev]");
  if (!button) return;
  const id = button.dataset.id;
  const action = button.dataset.dev;
  const peer = state.devices.peers.find((p) => p.device_id === id);

  switch (action) {
    case "pair":
      return startPairing(id);
    case "browse":
      // Browsing is a page, not a dialog: go there with this device chosen.
      state.network.deviceId = id;
      return switchPage("network");
    case "block": {
      if (!peer) return;
      const ok = await confirmDialog(
        "Block " + peer.name + "?",
        "It stays in your list so you can unblock it later, but it will not be able to browse your shares.",
        "Block"
      );
      if (!ok) return;
      await callQuiet("set_peer_blocked", { deviceId: id, blocked: true });
      return refreshDevices();
    }
    case "unblock":
      await callQuiet("set_peer_blocked", { deviceId: id, blocked: false });
      return refreshDevices();
    case "unpair": {
      if (!peer) return;
      const ok = await confirmDialog(
        "Unpair " + peer.name + "?",
        "Nothing is deleted. You will both need to pair again before you can send files to each other.",
        "Unpair"
      );
      if (!ok) return;
      await callQuiet("unpair_peer", { deviceId: id });
      return refreshDevices();
    }
    case "rename": {
      if (!peer) return;
      const name = window.prompt("Name for this device", peer.name);
      if (!name) return;
      await callQuiet("rename_peer", { deviceId: id, name });
      return refreshDevices();
    }
    case "cancel-transfer":
      await callQuiet("cancel_transfer", { transferId: Number(id) });
      return refreshDevices();
  }
}

// --- wiring ----------------------------------------------------------------

function wireDevices() {
  $("nearby-list").addEventListener("click", onDeviceAction);
  $("paired-list").addEventListener("click", onDeviceAction);

  $("self-name-save").addEventListener("click", async () => {
    const name = $("self-name").value.trim();
    if (!name) return;
    try {
      await call("set_device_name", { name });
      showToast("Renamed to " + name, "success");
      loadIdentity();
    } catch (_err) {
      /* already reported */
    }
  });

  // --- pairing dialogs ---
  $("pair-out-cancel").addEventListener("click", cancelPairing);
  $("pair-in-accept").addEventListener("click", () => answerIncomingPair(true));
  $("pair-in-reject").addEventListener("click", () => answerIncomingPair(false));

  // --- add by address ---
  $("add-ip-btn").addEventListener("click", () => {
    $("addip-input").value = "";
    $("addip-error").textContent = "";
    $("addip-backdrop").classList.remove("hidden");
    $("addip-input").focus();
  });
  $("addip-cancel").addEventListener("click", () => $("addip-backdrop").classList.add("hidden"));
  $("addip-ok").addEventListener("click", async () => {
    const address = $("addip-input").value.trim();
    if (!address) return;
    $("addip-error").textContent = "Looking…";
    try {
      const device = await callQuiet("add_peer_by_address", { address });
      if (!device) throw new Error("could not reach that address");
      $("addip-backdrop").classList.add("hidden");
      showToast("Found " + device.name, "success");
      refreshDevices();
    } catch (err) {
      $("addip-error").textContent = String(err.message || err);
    }
  });

  // Escape on an incoming request SNOOZES rather than answering: the request
  // stays pending and re-opens from the Devices page. Answering on Escape
  // would make a stray keypress a security decision.
  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    if (!$("pair-in-backdrop").classList.contains("hidden")) {
      $("pair-in-backdrop").classList.add("hidden");
      state.pairing.shownPairId = null;
    }
    if (!$("net-viewer").classList.contains("hidden")) {
      closeNetViewer();
      return;
    }
    if (!$("addip-backdrop").classList.contains("hidden")) {
      $("addip-backdrop").classList.add("hidden");
    }
  });
}

// ============================================================
// 19  Network — what other devices share
// ============================================================
//
// The mirror of the Shares page: that one is what you hand out, this one is
// what you can reach. Everything here reads; nothing on either side can push.
//
// Three things the desktop webview cannot do directly, and how each is solved:
//   - thumbnails need a bearer token an <img> cannot send, so `peer_thumb`
//     proxies them through Rust as data URLs, lazily, as tiles scroll in;
//   - full-size media would be absurd as base64, so `peer_media_url` mints a
//     play link on the far device and the tag points at a real URL, which is
//     what makes video seek;
//   - downloads run as background tasks and report through the same progress
//     machinery share indexing uses.

const NET_DOWNLOAD_POLL_MS = 400;

state.network = {
  deviceId: "",
  // null means "show that device's list of shares".
  shareId: null,
  shareName: "",
  path: "",
  entries: [],
  filter: "",
  mode: "grid",
  // Names ticked in the CURRENT folder. Cleared on navigation, because a
  // selection you cannot see is a selection you will download by accident.
  selected: new Set(),
  loading: false,
  error: "",
  downloads: [],
  viewerIndex: -1,
};

async function enterNetwork() {
  const peers = await callQuiet("list_peers");
  if (Array.isArray(peers)) state.devices.peers = peers;
  renderNetDevices();

  const usable = netUsableDevices();
  if (!usable.some((p) => p.device_id === state.network.deviceId)) {
    state.network.deviceId = usable.length ? usable[0].device_id : "";
    state.network.shareId = null;
    state.network.path = "";
  }
  $("net-device").value = state.network.deviceId;
  loadNetListing();
}

function netUsableDevices() {
  return state.devices.peers.filter((p) => !p.blocked);
}

function renderNetDevices() {
  const select = $("net-device");
  const usable = netUsableDevices();
  select.innerHTML = usable.length
    ? usable
        .map(
          (p) =>
            '<option value="' + escapeHtml(p.device_id) + '">' +
            escapeHtml(p.name) + (p.online ? "" : " (offline)") + "</option>"
        )
        .join("")
    : '<option value="">No paired devices</option>';
  select.disabled = !usable.length;
}

function netCrumbs() {
  const device = state.devices.peers.find((p) => p.device_id === state.network.deviceId);
  const parts = [device ? device.name : "—"];
  if (state.network.shareId) parts.push(state.network.shareName);
  if (state.network.path) parts.push(...state.network.path.split("/"));
  return parts.join(" › ");
}

async function loadNetListing() {
  const net = state.network;
  $("net-crumbs").textContent = netCrumbs();
  $("net-up").disabled = !net.shareId;

  if (!net.deviceId) {
    net.entries = [];
    net.error = "";
    renderNetwork();
    return;
  }

  net.loading = true;
  net.error = "";
  renderNetwork();

  let data;
  try {
    data = await call("peer_browse", {
      deviceId: net.deviceId,
      shareId: net.shareId,
      path: net.path,
    });
  } catch (err) {
    net.loading = false;
    net.entries = [];
    net.error = String(err.message || err);
    renderNetwork();
    return;
  }

  // Two shapes behind one call: a list of shares, or a folder listing.
  // Normalised here so everything downstream renders one kind of row.
  net.entries = net.shareId
    ? (data.entries || []).map((e) => ({
        name: e.name,
        isDir: e.is_dir,
        size: e.size,
        sizeText: e.size_text,
        kind: e.kind,
        ext: e.ext,
        thumb: !!e.thumb_url,
        playable: e.playable,
      }))
    : (data || []).map((share) => ({
        name: share.name,
        isDir: true,
        isShare: true,
        shareId: share.id,
        size: 0,
        sizeText: "",
        kind: "dir",
        ext: "",
        thumb: false,
        playable: false,
      }));

  net.loading = false;
  net.selected.clear();
  renderNetwork();
}

function netVisibleEntries() {
  const filter = state.network.filter.trim().toLowerCase();
  const rows = filter
    ? state.network.entries.filter((e) => e.name.toLowerCase().includes(filter))
    : state.network.entries;
  // Folders first, then by name — the order that makes a mixed folder
  // navigable, and the one the receiver already uses.
  return [...rows].sort((a, b) => {
    if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
    return a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: "base" });
  });
}

function netIcon(entry) {
  if (entry.isDir) return entry.isShare ? "folder_shared" : "folder";
  switch (entry.kind) {
    case "image":
      return "image";
    case "video":
      return "movie";
    case "audio":
      return "music_note";
    case "pdf":
      return "picture_as_pdf";
    case "archive":
      return "folder_zip";
    case "text":
      return "description";
    default:
      return "draft";
  }
}

function renderNetwork() {
  const host = $("net-body");
  const net = state.network;

  if (!netUsableDevices().length) {
    host.innerHTML = netEmpty(
      "devices",
      "No paired devices yet",
      "Pair a computer on the Devices page and whatever it shares will show up here."
    );
    return renderNetSelection();
  }
  if (net.loading) {
    host.innerHTML = netEmpty("hourglass_top", "Loading…", "");
    return renderNetSelection();
  }
  if (net.error) {
    host.innerHTML = netEmpty("wifi_off", "Could not reach that device", escapeHtml(net.error));
    return renderNetSelection();
  }

  const rows = netVisibleEntries();
  $("net-count").textContent = rows.length;

  if (!rows.length) {
    host.innerHTML = net.filter
      ? netEmpty("search_off", "Nothing matches that filter", "")
      : netEmpty(
          "folder_open",
          net.shareId ? "This folder is empty" : "That device shares nothing yet",
          ""
        );
    return renderNetSelection();
  }

  host.innerHTML =
    net.mode === "grid"
      ? '<div class="net-grid">' + rows.map(netTileHtml).join("") + "</div>"
      : '<div class="net-list">' + rows.map(netRowHtml).join("") + "</div>";

  renderNetSelection();
  hydrateNetThumbs();
}

function netEmpty(icon, title, body) {
  return `
    <div class="net-empty">
      <span class="material-symbols-outlined">${icon}</span>
      <span class="net-empty-title">${escapeHtml(title)}</span>
      ${body ? '<span class="net-empty-body">' + body + "</span>" : ""}
    </div>`;
}

function netCheckHtml(entry) {
  // A share is a whole folder tree on another machine; downloading one by
  // ticking a box is not a thing anyone means to do, so shares are not
  // selectable — open one and pick from inside it.
  if (entry.isShare) return "<span></span>";
  return `<span class="net-check" data-net="tick" data-name="${escapeHtml(entry.name)}">
            <span class="material-symbols-outlined">check</span>
          </span>`;
}

function netTileHtml(entry) {
  const selected = state.network.selected.has(entry.name);
  const thumb = entry.thumb
    ? `<span class="net-thumb" data-thumb="${escapeHtml(entry.name)}">
         <span class="material-symbols-outlined">image</span>
       </span>`
    : `<span class="net-thumb"><span class="material-symbols-outlined">${netIcon(entry)}</span></span>`;

  return `
    <div class="net-tile ${selected ? "is-selected" : ""}" data-net="open" data-name="${escapeHtml(entry.name)}">
      ${netCheckHtml(entry)}
      ${entry.playable ? '<span class="net-badge">play</span>' : ""}
      ${thumb}
      <span class="net-tile-name">${escapeHtml(entry.name)}</span>
      <span class="net-tile-sub">${escapeHtml(entry.isDir ? "Folder" : entry.sizeText || "")}</span>
    </div>`;
}

function netRowHtml(entry) {
  const selected = state.network.selected.has(entry.name);
  const download = entry.isDir
    ? ""
    : `<button class="icon-button" type="button" data-net="download-one" data-name="${escapeHtml(entry.name)}"
               aria-label="Download ${escapeHtml(entry.name)}" data-tip="Save this to your Downloads folder.">
         <span class="material-symbols-outlined">download</span>
       </button>`;

  return `
    <div class="net-row ${selected ? "is-selected" : ""}" data-net="open" data-name="${escapeHtml(entry.name)}">
      ${netCheckHtml(entry)}
      <span class="net-row-icon material-symbols-outlined">${netIcon(entry)}</span>
      <span class="net-row-name">${escapeHtml(entry.name)}</span>
      <span class="net-row-size">${escapeHtml(entry.isDir ? "" : entry.sizeText || "")}</span>
      <span class="net-row-actions">${download}</span>
    </div>`;
}

/// Fetch thumbnails only for tiles actually on screen.
///
/// A folder of 2,000 photos would otherwise be 2,000 round trips to the other
/// machine before the first one appears.
let netThumbObserver = null;

function hydrateNetThumbs() {
  if (netThumbObserver) netThumbObserver.disconnect();
  const targets = document.querySelectorAll("[data-thumb]");
  if (!targets.length) return;

  // Captured now: by the time a request lands the user may have navigated, and
  // a thumbnail from the previous folder must not be painted into this one.
  const net = state.network;
  const forDevice = net.deviceId;
  const forShare = net.shareId;
  const forPath = net.path;

  netThumbObserver = new IntersectionObserver(
    (items) => {
      for (const item of items) {
        if (!item.isIntersecting) continue;
        const host = item.target;
        netThumbObserver.unobserve(host);
        const name = host.dataset.thumb;
        const path = forPath ? forPath + "/" + name : name;
        callQuiet("peer_thumb", { deviceId: forDevice, shareId: forShare, path }).then((dataUrl) => {
          if (!dataUrl || !host.isConnected) return;
          if (net.deviceId !== forDevice || net.shareId !== forShare || net.path !== forPath) return;
          host.innerHTML = '<img src="' + dataUrl + '" alt="" />';
        });
      }
    },
    { rootMargin: "300px 0px" }
  );
  targets.forEach((t) => netThumbObserver.observe(t));
}

// --- selection -------------------------------------------------------------

function renderNetSelection() {
  const net = state.network;
  const count = net.selected.size;
  $("net-selection").classList.toggle("hidden", count === 0);
  $("net-selected-text").textContent = formatCount(count, "item") + " selected";
  const selectable = netVisibleEntries().filter((e) => !e.isShare);
  $("net-select-all").checked = count > 0 && count === selectable.length;
}

function toggleNetSelection(name) {
  const net = state.network;
  if (net.selected.has(name)) net.selected.delete(name);
  else net.selected.add(name);
  renderNetwork();
}

// --- navigation ------------------------------------------------------------

function netOpen(name) {
  const net = state.network;
  const entry = net.entries.find((e) => e.name === name);
  if (!entry) return;

  if (entry.isShare) {
    net.shareId = entry.shareId;
    net.shareName = entry.name;
    net.path = "";
    return loadNetListing();
  }
  if (entry.isDir) {
    net.path = net.path ? net.path + "/" + entry.name : entry.name;
    return loadNetListing();
  }

  // A file. Preview what can be previewed; anything else, the click means
  // "I want this", so fetch it.
  if (entry.kind === "image" || entry.kind === "video" || entry.kind === "audio") {
    const index = netViewable().findIndex((e) => e.name === name);
    if (index >= 0) return openNetViewer(index);
  }
  startNetDownload([entry], false);
}

function netUp() {
  const net = state.network;
  if (net.path) {
    const idx = net.path.lastIndexOf("/");
    net.path = idx < 0 ? "" : net.path.slice(0, idx);
  } else {
    net.shareId = null;
    net.shareName = "";
  }
  loadNetListing();
}

// --- downloads -------------------------------------------------------------

function netEntryPath(entry) {
  return state.network.path ? state.network.path + "/" + entry.name : entry.name;
}

async function startNetDownload(entries, zip) {
  const net = state.network;
  if (!entries.length || !net.shareId) return;

  const items = entries.map((e) => ({
    path: netEntryPath(e),
    name: e.name,
    is_dir: !!e.isDir,
    size: e.size || 0,
  }));

  let handle;
  try {
    handle = await call("start_peer_download_task", {
      deviceId: net.deviceId,
      shareId: net.shareId,
      items,
      zip,
    });
  } catch (_err) {
    return; // call() already reported it
  }

  const label =
    entries.length === 1
      ? entries[0].name + (zip ? " (zip)" : "")
      : formatCount(entries.length, "item") + (zip ? " as .zip" : "");

  net.downloads.unshift({
    id: handle.id,
    label,
    message: "Starting…",
    progress: 0,
    done: false,
    error: null,
  });
  renderNetDownloads();
  pollNetDownload(handle.id);
}

async function pollNetDownload(taskId) {
  while (true) {
    await sleep(NET_DOWNLOAD_POLL_MS);
    const payload = await callQuiet("get_task_progress", { taskId });
    const row = state.network.downloads.find((d) => d.id === taskId);
    if (!row) return; // the user cleared it

    if (!payload) {
      row.done = true;
      row.error = "lost contact with the download";
      renderNetDownloads();
      return;
    }

    row.progress = payload.progress || 0;
    row.message = payload.message || "";

    if (payload.done) {
      row.done = true;
      row.error = payload.error || null;
      if (payload.error) {
        showToast(payload.error, "error");
      } else {
        const where = (payload.download_result && payload.download_result.path) || "";
        showToast("Saved to " + where, "success");
        addLog("Downloaded " + row.label + " → " + where);
      }
      renderNetDownloads();
      await callQuiet("clear_task", { taskId });
      return;
    }
    renderNetDownloads();
  }
}

function renderNetDownloads() {
  const rows = state.network.downloads;
  $("net-downloads-card").classList.toggle("hidden", rows.length === 0);
  $("net-downloads").innerHTML = rows
    .map((d) => {
      const pct = Math.round((d.progress || 0) * 100);
      const icon = d.error ? "error" : d.done ? "check_circle" : "download";
      // No declared total on a streamed archive, so the bar goes indeterminate
      // rather than sitting at zero and looking stuck.
      const bar = d.done
        ? ""
        : `<span class="progress-track">
             <span class="progress-bar${pct ? "" : " is-indeterminate"}" style="width:${pct}%"></span>
           </span>`;
      return `
      <div class="net-download-row ${d.done ? "is-done" : ""}">
        <span class="material-symbols-outlined">${icon}</span>
        <span class="net-download-main">
          <span class="net-download-name">${escapeHtml(d.label)}</span>
          ${bar}
          <span class="net-download-meta">${escapeHtml(d.error || d.message || "")}</span>
        </span>
        <span></span>
      </div>`;
    })
    .join("");
}

// --- preview ---------------------------------------------------------------

function netViewable() {
  return netVisibleEntries().filter(
    (e) => !e.isDir && (e.kind === "image" || e.kind === "video" || e.kind === "audio")
  );
}

function netViewerEntry() {
  return netViewable()[state.network.viewerIndex] || null;
}

async function openNetViewer(index) {
  const rows = netViewable();
  if (index < 0 || index >= rows.length) return;
  const entry = rows[index];
  const net = state.network;
  net.viewerIndex = index;

  $("net-viewer").classList.remove("hidden");
  $("net-viewer-name").textContent = entry.name;
  $("net-viewer-prev").classList.toggle("hidden", index <= 0);
  $("net-viewer-next").classList.toggle("hidden", index >= rows.length - 1);

  const stage = $("net-viewer-stage");
  stage.innerHTML = '<div class="net-viewer-fallback"><span>Loading…</span></div>';

  // .mkv and friends: the file is fine, a browser is the wrong software. Say so
  // and point at the two things that do work.
  if (!entry.playable && entry.kind !== "image") {
    stage.innerHTML = `
      <div class="net-viewer-fallback">
        <span>Browsers can't play ${escapeHtml((entry.ext || "").toUpperCase())}</span>
        <span>Use Open in player, or download it.</span>
      </div>`;
    return;
  }

  let url;
  try {
    url = await call("peer_media_url", {
      deviceId: net.deviceId,
      shareId: net.shareId,
      path: netEntryPath(entry),
    });
  } catch (_err) {
    stage.innerHTML = '<div class="net-viewer-fallback"><span>Could not open that file</span></div>';
    return;
  }
  // Stale answer: the user stepped on while this was in flight.
  if (net.viewerIndex !== index) return;

  if (entry.kind === "image") {
    stage.innerHTML = `<img src="${escapeHtml(url)}" alt="${escapeHtml(entry.name)}" />`;
  } else if (entry.kind === "video") {
    stage.innerHTML = `<video src="${escapeHtml(url)}" controls autoplay preload="metadata"></video>`;
  } else {
    stage.innerHTML = `<audio src="${escapeHtml(url)}" controls autoplay preload="metadata"></audio>`;
  }
}

function closeNetViewer() {
  state.network.viewerIndex = -1;
  $("net-viewer").classList.add("hidden");
  // Emptying the stage stops playback: a hidden <video> keeps pulling from the
  // other machine otherwise.
  $("net-viewer-stage").innerHTML = "";
}

function stepNetViewer(delta) {
  const next = state.network.viewerIndex + delta;
  if (next < 0 || next >= netViewable().length) return;
  openNetViewer(next);
}

// --- wiring ----------------------------------------------------------------

function wireNetwork() {
  $("net-device").addEventListener("change", (event) => {
    const net = state.network;
    net.deviceId = event.target.value;
    net.shareId = null;
    net.shareName = "";
    net.path = "";
    loadNetListing();
  });

  $("net-up").addEventListener("click", netUp);

  $("net-filter").addEventListener(
    "input",
    debounce((event) => {
      state.network.filter = event.target.value;
      renderNetwork();
    }, 140)
  );

  $("net-view-mode").addEventListener("click", (event) => {
    const button = event.target.closest("[data-net-mode]");
    if (!button) return;
    state.network.mode = button.dataset.netMode;
    $("net-view-mode")
      .querySelectorAll(".mode-button")
      .forEach((b) => b.classList.toggle("active", b === button));
    renderNetwork();
  });

  $("net-body").addEventListener("click", (event) => {
    const target = event.target.closest("[data-net]");
    if (!target) return;
    const name = target.dataset.name;
    switch (target.dataset.net) {
      case "tick":
        // The tick sits on top of the tile, which is itself a click target.
        event.stopPropagation();
        return toggleNetSelection(name);
      case "download-one": {
        event.stopPropagation();
        const entry = state.network.entries.find((e) => e.name === name);
        if (entry) startNetDownload([entry], false);
        return;
      }
      case "open":
        return netOpen(name);
    }
  });

  $("net-select-all").addEventListener("change", (event) => {
    const net = state.network;
    net.selected.clear();
    if (event.target.checked) {
      netVisibleEntries()
        .filter((e) => !e.isShare)
        .forEach((e) => net.selected.add(e.name));
    }
    renderNetwork();
  });

  $("net-clear-selection").addEventListener("click", () => {
    state.network.selected.clear();
    renderNetwork();
  });

  const selectedEntries = () =>
    state.network.entries.filter((e) => state.network.selected.has(e.name));
  $("net-download").addEventListener("click", () => startNetDownload(selectedEntries(), false));
  $("net-download-zip").addEventListener("click", () => startNetDownload(selectedEntries(), true));

  $("net-downloads-clear").addEventListener("click", () => {
    state.network.downloads = state.network.downloads.filter((d) => !d.done);
    renderNetDownloads();
  });

  // --- preview ---
  $("net-viewer-close").addEventListener("click", closeNetViewer);
  $("net-viewer-prev").addEventListener("click", () => stepNetViewer(-1));
  $("net-viewer-next").addEventListener("click", () => stepNetViewer(1));

  $("net-viewer-download").addEventListener("click", () => {
    const entry = netViewerEntry();
    if (entry) startNetDownload([entry], false);
  });

  $("net-viewer-play").addEventListener("click", async () => {
    const entry = netViewerEntry();
    if (!entry) return;
    try {
      await call("play_peer_file", {
        deviceId: state.network.deviceId,
        shareId: state.network.shareId,
        path: netEntryPath(entry),
      });
      showToast("Opening " + entry.name + " in your player", "success");
    } catch (_err) {
      /* already reported */
    }
  });

  document.addEventListener("keydown", (event) => {
    if ($("net-viewer").classList.contains("hidden")) return;
    if (event.key === "ArrowLeft") stepNetViewer(-1);
    else if (event.key === "ArrowRight") stepNetViewer(1);
  });
}

// ============================================================
// 20  Tooltips
// ============================================================

/* Anything wearing `data-tip` explains itself on hover and on keyboard focus.
   Three decisions worth writing down:

   One element, positioned from JS against `document.body`, rather than a CSS
   `::after` on each control. Half the buttons in this app live inside
   `.shares-table-wrap` and the dialogs, which scroll and therefore clip; a
   pseudo-element tooltip on a table-row button is cut off exactly where it is
   needed most.

   `data-tip`, not `title`. The native tooltip appears after about a second, in
   the OS font, and never on keyboard focus — three reasons it is no use for
   telling someone what a button does.

   Delegated from `document`, so every row rendered later (shares, devices,
   transfers, sessions) is covered by writing one attribute in its template. */

const TOOLTIP_DELAY_MS = 320;
/// Distance between the control and the bubble, and the minimum gap kept from
/// the window edge.
const TOOLTIP_GAP = 8;

let tooltipEl = null;
let tooltipTimer = null;
/// The control the tip belongs to, whether already shown or still pending. Kept
/// so that moving the pointer within one button does not restart the delay
/// forever — which is the classic way a delayed tooltip never appears at all.
let tooltipFor = null;
/// The text on screen right now, "" when nothing is shown. Used to re-anchor
/// silently when a list re-renders the very element being explained.
let tooltipText = "";

function tooltipHost() {
  if (!tooltipEl) {
    tooltipEl = document.createElement("div");
    tooltipEl.className = "tooltip";
    // Purely visual. Every control it explains already carries its own
    // accessible name, and announcing this too would just say it twice.
    tooltipEl.setAttribute("aria-hidden", "true");
    document.body.appendChild(tooltipEl);
  }
  return tooltipEl;
}

function hideTooltip() {
  if (tooltipTimer) clearTimeout(tooltipTimer);
  tooltipTimer = null;
  tooltipFor = null;
  tooltipText = "";
  if (tooltipEl) tooltipEl.classList.remove("is-visible");
}

function showTooltip(target) {
  const text = target.dataset.tip;
  // A row can be re-rendered out from under a pending timer.
  if (!text || !target.isConnected) return hideTooltip();

  tooltipFor = target;
  tooltipText = text;
  const host = tooltipHost();
  host.textContent = text;
  host.classList.add("is-visible");

  // Measure after the text is in, or the first tip of a session is placed
  // using the previous one's size.
  const anchor = target.getBoundingClientRect();
  const bubble = host.getBoundingClientRect();

  // Above by preference; below when the control is near the top of the window.
  let top = anchor.top - bubble.height - TOOLTIP_GAP;
  if (top < TOOLTIP_GAP) top = anchor.bottom + TOOLTIP_GAP;

  let left = anchor.left + anchor.width / 2 - bubble.width / 2;
  const rightLimit = window.innerWidth - bubble.width - TOOLTIP_GAP;
  left = Math.max(TOOLTIP_GAP, Math.min(left, Math.max(TOOLTIP_GAP, rightLimit)));

  host.style.top = Math.round(top) + "px";
  host.style.left = Math.round(left) + "px";
}

/// The nearest ancestor carrying a tip, or null. Guarded because a bubbling
/// event's target is not always an element.
function tipTargetFrom(node) {
  return node && node.closest ? node.closest("[data-tip]") : null;
}

/// `:focus-visible` is what separates "tabbed to" from "clicked on". A webview
/// too old to know the selector throws rather than returning false, and losing
/// every keyboard tip is worse than an occasional flash under a click.
function isKeyboardFocus(node) {
  try {
    return node.matches(":focus-visible");
  } catch (_err) {
    return true;
  }
}

function wireTooltips() {
  document.addEventListener("mouseover", (event) => {
    const target = tipTargetFrom(event.target);
    if (!target || target === tooltipFor) return;

    // The device and transfer lists re-render every 1.5 s, replacing the very
    // element under a stationary pointer. Same words, same place: re-anchor
    // silently. Hiding and re-timing it would blink once per refresh.
    if (tooltipText && target.dataset.tip === tooltipText) {
      showTooltip(target);
      return;
    }

    hideTooltip();
    tooltipFor = target;
    // The delay is what keeps tips out of the way while the pointer is merely
    // crossing a toolbar. They should answer a question, not shout.
    tooltipTimer = setTimeout(() => showTooltip(target), TOOLTIP_DELAY_MS);
  });

  document.addEventListener("mouseout", (event) => {
    const from = tipTargetFrom(event.target);
    if (!from) return;
    // Moving from a button onto its own icon is not leaving the button.
    if (tipTargetFrom(event.relatedTarget) === from) return;
    hideTooltip();
  });

  // Keyboard: a tip that only answers to the mouse is invisible to anyone
  // tabbing through. Focus is deliberate, so it skips the delay.
  document.addEventListener("focusin", (event) => {
    const target = tipTargetFrom(event.target);
    if (!target) return;
    // Keyboard focus only. A click focuses too, and flashing the tip under the
    // cursor mid-click is noise.
    if (!isKeyboardFocus(event.target)) return;
    showTooltip(target);
  });
  document.addEventListener("focusout", hideTooltip);

  // A tip must not outlive what it points at. Capture phase, because the click
  // handler underneath usually re-renders the row and takes the element away.
  document.addEventListener("click", hideTooltip, true);
  document.addEventListener("scroll", hideTooltip, true);
  window.addEventListener("resize", hideTooltip);
  window.addEventListener("blur", hideTooltip);
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") hideTooltip();
  });
}
