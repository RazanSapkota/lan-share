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
    14  Preview page
    15  Activity page
    16  Settings page
    17  Event wiring + boot
    18  Devices — discovery, pairing, transfers
   ========================================================= */

// ============================================================
// 1  Runtime handle + constants + state
// ============================================================

const invoke = window.__TAURI__?.core?.invoke;

const TASK_POLL_MS = 300;
const SERVER_POLL_MS = 1000;
const ACTIVITY_POLL_MS = 1500;
const ACTIVITY_LIMIT = 300;

const PAGES = ["dashboard", "shares", "devices", "preview", "activity", "settings"];

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

  preview: { shareId: "", url: "" },
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
  devices: () => {
    loadIdentity();
    loadHandoffs();
    startDevicesTick();
  },
  preview: () => renderPreview(),
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
  renderPreviewOptions();
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
         <span class="link-text" title="${escapeHtml(link)}">${escapeHtml(revealed ? link : maskLink(link))}</span>
         <button class="icon-button" type="button" data-act="reveal" data-id="${escapeHtml(share.id)}" aria-label="Show link">
           <span class="material-symbols-outlined">${revealed ? "visibility_off" : "visibility"}</span>
         </button>
         <button class="icon-button" type="button" data-act="copy-link" data-id="${escapeHtml(share.id)}" aria-label="Copy link">
           <span class="material-symbols-outlined">content_copy</span>
         </button>
         <button class="icon-button" type="button" data-act="qr" data-id="${escapeHtml(share.id)}" aria-label="Show QR code">
           <span class="material-symbols-outlined">qr_code_2</span>
         </button>
       </div>`
    : '<span class="text-dim">Start the server</span>';

  return `
    <tr class="${share.enabled ? "" : "is-disabled"}">
      <td>
        <button class="switch ${share.enabled ? "is-on" : ""}" type="button"
                data-act="toggle" data-id="${escapeHtml(share.id)}"
                role="switch" aria-checked="${share.enabled}"
                aria-label="Share ${escapeHtml(share.name)}"></button>
      </td>
      <td>
        <div class="sh-name">${escapeHtml(share.name)}
          ${share.is_inbox ? '<span class="badge badge-inbox">Inbox</span>' : ""}
          ${share.is_file ? '<span class="badge">File</span>' : ""}
        </div>
        ${share.note ? '<div class="text-dim" style="font-size:11.5px">' + escapeHtml(share.note) + "</div>" : ""}
      </td>
      <td><div class="sh-path" title="${escapeHtml(share.display_path)}">${escapeHtml(share.display_path)}</div></td>
      <td class="sh-cell-num">${contents}</td>
      <td>${linkCell}</td>
      <td>${status}</td>
      <td class="sh-cell-actions">
        <button class="icon-button" type="button" data-act="index" data-id="${escapeHtml(share.id)}" aria-label="Count files">
          <span class="material-symbols-outlined">calculate</span>
        </button>
        <button class="icon-button" type="button" data-act="open" data-id="${escapeHtml(share.id)}" aria-label="Show in file manager">
          <span class="material-symbols-outlined">folder_open</span>
        </button>
        <button class="icon-button" type="button" data-act="edit" data-id="${escapeHtml(share.id)}" aria-label="Edit share">
          <span class="material-symbols-outlined">tune</span>
        </button>
        <button class="icon-button" type="button" data-act="regen" data-id="${escapeHtml(share.id)}" aria-label="New secret link">
          <span class="material-symbols-outlined">key</span>
        </button>
        <button class="icon-button" type="button" data-act="remove" data-id="${escapeHtml(share.id)}" aria-label="Stop sharing">
          <span class="material-symbols-outlined">delete</span>
        </button>
      </td>
    </tr>`;
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

function renderPreviewOptions() {
  const select = $("preview-share");
  if (!select) return;
  const shares = state.shares.items.filter((s) => s.enabled);
  select.innerHTML = shares.length
    ? shares.map((s) => `<option value="${escapeHtml(s.id)}">${escapeHtml(s.name)}</option>`).join("")
    : '<option value="">No shares</option>';
  if (state.preview.shareId) select.value = state.preview.shareId;
  else if (shares.length) state.preview.shareId = shares[0].id;
}

function renderPreview() {
  const wrap = $("preview-frame-wrap");
  const placeholder = $("preview-placeholder");
  const share = state.shares.items.find((s) => s.id === $("preview-share").value);

  if (!state.server.running) {
    wrap.classList.add("hidden");
    placeholder.classList.remove("hidden");
    $("preview-placeholder-text").textContent = "Start the server to preview it.";
    $("preview-url").textContent = "";
    return;
  }
  if (!share) {
    wrap.classList.add("hidden");
    placeholder.classList.remove("hidden");
    $("preview-placeholder-text").textContent = "Add a folder on the Shares page first.";
    $("preview-url").textContent = "";
    return;
  }

  // Loopback, not the LAN address: this must work even when the firewall is
  // blocking external connections, which is precisely when you want to look.
  const url = "http://127.0.0.1:" + state.server.port + "/s/" + share.token;
  state.preview.url = url;
  state.preview.shareId = share.id;
  $("preview-url").textContent = url;
  $("preview-frame").src = url;
  wrap.classList.remove("hidden");
  placeholder.classList.add("hidden");
}

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
    if (filter === "download") return e.kind === "download" || e.kind === "view" || e.kind === "upload";
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
          <div class="act-cell-file" title="${escapeHtml(entry.path || "")}">${escapeHtml(entry.path || "—")}</div>
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
        <button class="icon-button" type="button" data-session="${escapeHtml(s.token)}" aria-label="Sign this device out">
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

  // --- preview ---
  $("preview-share").addEventListener("change", renderPreview);
  $("preview-reload").addEventListener("click", () => {
    const frame = $("preview-frame");
    // Re-assigning the same src does not always reload; clearing it first does.
    frame.src = "about:blank";
    setTimeout(renderPreview, 40);
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
    "set-view",
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
  discovery: { running: false, health: "ok", error: null, devices: [] },
  peers: [],
  filter: "",
  tickTimer: null,
};

state.transfers = {
  items: [],
  // Per-transfer sampling for the speed readout. Outside `items` so a refresh
  // that replaces the rows does not reset every rate to zero.
  rates: {},
};

state.pairing = {
  // Outgoing: the task we are polling, and the last payload seen.
  taskId: null,
  cancelled: false,
  // Incoming: the prompt currently on screen, so a re-poll does not reopen it.
  shownPairId: null,
  acceptArmedAt: 0,
};

state.offers = { shownId: null, items: [] };
state.browse = { peerId: null, peerName: "", shareId: null, path: "" };

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
  const [discovery, peers, transfers] = await Promise.all([
    callQuiet("list_discovered"),
    callQuiet("list_peers"),
    callQuiet("list_transfers"),
  ]);
  if (discovery) state.devices.discovery = discovery;
  if (Array.isArray(peers)) state.devices.peers = peers;
  if (Array.isArray(transfers)) mergeTransfers(transfers);

  renderDiscovered();
  renderPeers();
  renderTransfers();
  renderDiscoveryNotice();
}

/// Runs on every page, forever. These two are requests from another human
/// waiting on an answer; missing one because you were on Settings is the
/// failure mode.
function startWatchTick() {
  setInterval(async () => {
    const [requests, offers] = await Promise.all([
      callQuiet("list_incoming_pair_requests"),
      callQuiet("list_incoming_offers"),
    ]);
    if (Array.isArray(requests)) showIncomingPair(requests);
    if (Array.isArray(offers)) showIncomingOffer(offers);
    updateDevicesBadge(
      (Array.isArray(requests) ? requests.length : 0) +
        (Array.isArray(offers) ? offers.length : 0)
    );
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
  $("self-discoverable").checked = identity.discoverable;
  $("self-receive").textContent = identity.receive_dir || "—";
  $("self-addr").textContent = identity.addresses.length
    ? identity.addresses[0]
    : "no network";
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
  const running = state.devices.discovery.running;
  radar.classList.toggle("is-scanning", running);

  // Already-paired devices belong in the list below. Showing them in both
  // invites re-pairing something already trusted.
  const rows = dedupeDiscovered(state.devices.discovery.devices || []).filter(
    (d) => !d.paired
  );
  $("nearby-count").textContent = rows.length;

  if (!rows.length) {
    host.innerHTML = `
      <div class="device-empty">
        <span class="material-symbols-outlined">${running ? "wifi_tethering" : "wifi_off"}</span>
        <span class="device-empty-title">${
          running ? "Looking for devices…" : "Discovery is off"
        }</span>
        <span class="device-empty-body">${
          running
            ? "Open LAN Share on another computer on this Wi-Fi and it will appear here."
            : "Start the server and turn on “Visible to other devices”."
        }</span>
      </div>`;
    return;
  }

  host.innerHTML = rows
    .map((d) => {
      const extra = d.addresses.length > 1
        ? `<span class="badge" title="${escapeHtml(d.addresses.join(", "))}">+${d.addresses.length - 1} more</span>`
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
            ${d.manual ? '<span class="badge">Added by hand</span>' : ""}
          </span>
        </span>
        <span class="device-auto is-hidden-soft"></span>
        <span class="device-actions">
          <button class="primary-button" type="button" data-dev="pair" data-id="${escapeHtml(d.device_id)}" ${d.online ? "" : "disabled"}>
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
    ? `<button class="ghost-button" type="button" data-dev="unblock" data-id="${id}">Unblock</button>
       <button class="icon-button" type="button" data-dev="unpair" data-id="${id}" aria-label="Unpair">
         <span class="material-symbols-outlined">link_off</span>
       </button>`
    : `<button class="primary-button" type="button" data-dev="send" data-id="${id}" ${p.online ? "" : "disabled"}>
         <span class="material-symbols-outlined">send</span><span>Send</span>
       </button>
       <button class="ghost-button" type="button" data-dev="browse" data-id="${id}" ${p.online ? "" : "disabled"}>
         <span class="material-symbols-outlined">folder_open</span><span>Browse</span>
       </button>
       <button class="icon-button" type="button" data-dev="rename" data-id="${id}" aria-label="Rename">
         <span class="material-symbols-outlined">edit</span>
       </button>
       <button class="icon-button" type="button" data-dev="block" data-id="${id}" aria-label="Block">
         <span class="material-symbols-outlined">block</span>
       </button>
       <button class="icon-button" type="button" data-dev="unpair" data-id="${id}" aria-label="Unpair">
         <span class="material-symbols-outlined">link_off</span>
       </button>`;

  return `
    <div class="device-row ${p.online ? "is-online" : ""} ${p.blocked ? "is-blocked" : ""}">
      <span class="device-icon"><span class="material-symbols-outlined">${deviceIcon(p.platform)}</span></span>
      <span class="device-body">
        <span class="device-name">${escapeHtml(p.name)}
          ${p.auto_accept && !p.blocked ? '<span class="badge badge-auto">Auto</span>' : ""}
        </span>
        <span class="device-sub">
          ${status}
          <span class="mono">${escapeHtml(p.address || "")}</span>
        </span>
      </span>
      <span class="device-auto ${p.blocked ? "is-hidden-soft" : ""}">
        <span class="device-auto-label">Always accept</span>
        <button class="switch ${p.auto_accept ? "is-on" : ""}" type="button"
                data-dev="auto" data-id="${id}" role="switch"
                aria-checked="${p.auto_accept}" aria-label="Always accept files from ${escapeHtml(p.name)}"></button>
      </span>
      <span class="device-actions">${actions}</span>
    </div>`;
}

// --- transfers -------------------------------------------------------------

/// Merge rather than replace, so the sampled rate survives a refresh.
function mergeTransfers(rows) {
  const now = Date.now();
  for (const row of rows) {
    const previous = state.transfers.items.find((t) => t.id === row.id);
    if (previous && row.bytes > previous.bytes) {
      const seconds = (now - (state.transfers.rates[row.id]?.at || now)) / 1000;
      if (seconds > 0.4) {
        state.transfers.rates[row.id] = {
          at: now,
          bps: (row.bytes - previous.bytes) / seconds,
        };
      }
    } else if (!state.transfers.rates[row.id]) {
      state.transfers.rates[row.id] = { at: now, bps: 0 };
    }
  }
  state.transfers.items = rows;
}

function renderTransfers() {
  const host = $("transfers-list");
  const rows = state.transfers.items;
  $("transfers-count").textContent = rows.length;

  if (!rows.length) {
    host.innerHTML = `
      <div class="device-empty">
        <span class="material-symbols-outlined">swap_vert</span>
        <span class="device-empty-title">No transfers yet</span>
      </div>`;
    return;
  }
  host.innerHTML = rows.map(transferRowHtml).join("");
}

function transferRowHtml(t) {
  const out = t.direction === "out";
  const filePct = t.file_total_bytes
    ? Math.min(100, Math.round((t.file_bytes / t.file_total_bytes) * 100))
    : 0;
  const totalPct = t.total_bytes
    ? Math.min(100, Math.round((t.bytes / t.total_bytes) * 100))
    : 0;

  let meta;
  if (t.status === "active") {
    const bps = state.transfers.rates[t.id]?.bps || 0;
    meta = [
      formatBytes(t.bytes) + " of " + formatBytes(t.total_bytes),
      formatRate(bps),
      formatEta(t.total_bytes - t.bytes, bps),
    ]
      .filter(Boolean)
      .join(" · ");
  } else if (t.status === "failed") {
    // Name the device, not the socket: "connection reset by peer" is not
    // something a person can act on.
    meta = `<span class="transfer-bad">${escapeHtml(t.error || "Failed")}</span>`;
  } else if (t.status === "cancelled") {
    meta = "Cancelled";
  } else if (t.status === "declined") {
    meta = "Declined";
  } else {
    meta = formatBytes(t.bytes) + " · done";
  }

  const canCancel = t.status === "active";

  return `
    <div class="transfer-row is-${escapeHtml(t.status)}">
      <span class="transfer-dir ${out ? "is-out" : "is-in"}"
            title="${out ? "Sending" : "Receiving"}">
        <span class="material-symbols-outlined">${out ? "north_east" : "south_west"}</span>
      </span>
      <span class="transfer-main">
        <span class="transfer-head">
          <span class="transfer-peer">${escapeHtml(t.peer_name)}</span>
          <span class="transfer-file">${escapeHtml(t.file_name || "")}</span>
          <span class="spacer"></span>
          <span class="transfer-idx">${t.file_count > 1 ? `${t.file_index + 1}/${t.file_count}` : ""}</span>
        </span>
        <span class="progress-track transfer-track-file" role="progressbar"
              aria-label="Current file" aria-valuenow="${filePct}" aria-valuemin="0" aria-valuemax="100">
          <span class="progress-bar" style="width:${filePct}%"></span>
        </span>
        <span class="progress-track transfer-track-total" role="progressbar"
              aria-label="All files" aria-valuenow="${totalPct}" aria-valuemin="0" aria-valuemax="100">
          <span class="progress-bar" style="width:${totalPct}%"></span>
        </span>
        <span class="transfer-meta">${meta}</span>
      </span>
      <span class="device-actions">
        ${canCancel
          ? `<button class="icon-button" type="button" data-dev="cancel-transfer" data-id="${t.id}" aria-label="Cancel">
               <span class="material-symbols-outlined">close</span>
             </button>`
          : ""}
      </span>
    </div>`;
}

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

function showIncomingOffer(offers) {
  const backdrop = $("offer-backdrop");
  state.offers.items = offers;
  if (!offers.length) {
    if (state.offers.shownId) {
      backdrop.classList.add("hidden");
      state.offers.shownId = null;
    }
    return;
  }
  const offer = offers[0];
  if (state.offers.shownId === offer.offer_id) return;
  state.offers.shownId = offer.offer_id;

  $("offer-peer").textContent = offer.peer_name;
  $("offer-summary-text").textContent =
    offer.file_count + (offer.file_count === 1 ? " file" : " files") +
    " · " + offer.total_bytes_text;
  $("offer-dest").textContent = state.devices.self ? state.devices.self.receive_dir : "";
  $("offer-always").checked = false;

  const preview = offer.files.slice(0, 8);
  $("offer-files").innerHTML =
    preview
      .map(
        (f) => `<li class="offer-file">
          <span class="offer-file-name">${escapeHtml(f.name)}</span>
          <span class="offer-file-size">${formatBytes(f.size)}</span>
        </li>`
      )
      .join("") +
    (offer.file_count > preview.length
      ? `<li class="offer-file offer-file-more">and ${offer.file_count - preview.length} more</li>`
      : "");

  backdrop.classList.remove("hidden");
  $("offer-decline").focus();
}

async function answerOffer(accept) {
  const offerId = state.offers.shownId;
  if (!offerId) return;
  $("offer-backdrop").classList.add("hidden");
  state.offers.shownId = null;
  try {
    if (accept) {
      await call("accept_offer", {
        offerId,
        alwaysAccept: $("offer-always").checked,
      });
    } else {
      await call("decline_offer", { offerId });
    }
  } catch (_err) {
    /* already reported */
  }
  refreshDevices();
}

// --- sending ---------------------------------------------------------------

async function sendToDevice(deviceId) {
  const paths = await callQuiet("pick_files");
  if (!paths || !paths.length) return;
  try {
    const handle = await call("start_send_files_task", { deviceId, paths });
    addLog("Sending " + formatCount(paths.length, "file"));
    // The transfers panel is the progress UI; the task is polled only so a
    // failure surfaces as a toast rather than silently.
    pollSendTask(handle.id);
  } catch (_err) {
    /* already reported */
  }
}

async function pollSendTask(taskId) {
  try {
    while (true) {
      await sleep(TASK_POLL_MS);
      const payload = await callQuiet("get_task_progress", { taskId });
      if (!payload) return;
      refreshDevices();
      if (payload.error) {
        showToast(payload.error, "error");
        return;
      }
      if (payload.done) {
        showToast(payload.message || "Sent", "success");
        return;
      }
    }
  } finally {
    await callQuiet("clear_task", { taskId });
  }
}

// --- browse ----------------------------------------------------------------

async function openBrowse(deviceId) {
  const peer = state.devices.peers.find((p) => p.device_id === deviceId);
  state.browse = {
    peerId: deviceId,
    peerName: peer ? peer.name : "device",
    shareId: null,
    path: "",
  };
  $("browse-peer").textContent = state.browse.peerName;
  $("browse-backdrop").classList.remove("hidden");
  await loadBrowse();
}

async function loadBrowse() {
  const { peerId, shareId, path } = state.browse;
  const host = $("browse-list");
  host.innerHTML = '<div class="device-empty"><span class="device-empty-title">Loading…</span></div>';
  $("browse-path").textContent = shareId ? "/" + path : "Shared folders";
  $("browse-up").disabled = !shareId;

  let data;
  try {
    data = await call("peer_browse", { deviceId: peerId, shareId, path });
  } catch (_err) {
    host.innerHTML = '<div class="device-empty"><span class="device-empty-title">Could not reach that device</span></div>';
    return;
  }

  // Share list, or a directory listing — the shapes differ.
  const rows = shareId ? data.entries || [] : data || [];
  if (!rows.length) {
    host.innerHTML = '<div class="device-empty"><span class="device-empty-title">Nothing here</span></div>';
    return;
  }

  host.innerHTML = rows
    .map((row) => {
      const isDir = shareId ? row.is_dir : true;
      const name = shareId ? row.name : row.name;
      const target = shareId ? row.name : row.id;
      return `
      <div class="browse-row ${isDir ? "is-dir" : ""}" data-browse="${isDir ? "open" : "file"}"
           data-target="${escapeHtml(target)}">
        <span class="material-symbols-outlined browse-icon">${isDir ? "folder" : "draft"}</span>
        <span class="browse-name">${escapeHtml(name)}</span>
        <span class="browse-size">${shareId && !row.is_dir ? escapeHtml(row.size_text) : ""}</span>
        <span></span>
      </div>`;
    })
    .join("");
}

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
    case "send":
      return sendToDevice(id);
    case "browse":
      return openBrowse(id);
    case "auto": {
      if (!peer) return;
      // Asked only when turning it ON, because that removes a prompt. Turning
      // it off restores one and needs no confirmation.
      if (!peer.auto_accept) {
        const ok = await confirmDialog(
          "Always accept files from " + peer.name + "?",
          "Files from this device will be saved without asking you first.",
          "Always accept"
        );
        if (!ok) return;
      }
      await callQuiet("set_peer_auto_accept", { deviceId: id, enabled: !peer.auto_accept });
      return refreshDevices();
    }
    case "block": {
      if (!peer) return;
      const ok = await confirmDialog(
        "Block " + peer.name + "?",
        "It stays in your list so you can unblock it later, but it will not be able to send you files or browse your shares.",
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

// --- send to phone ---------------------------------------------------------

/// A handoff link is short-lived, so how long is left is the useful fact — the
/// file count is already in the label.
function relativeExpiry(ms) {
  const left = Number(ms) - Date.now();
  if (left <= 0) return "expired";
  if (left < 60_000) return "expires in under a minute";
  return "expires in " + Math.round(left / 60_000) + "m";
}

async function createHandoff() {
  const paths = await callQuiet("pick_files");
  if (!paths || !paths.length) return;
  try {
    const handoff = await call("create_handoff", { paths, minutes: 60 });
    showHandoff(handoff);
    loadHandoffs();
  } catch (_err) {
    /* already reported */
  }
}

function showHandoff(handoff) {
  $("handoff-qr").innerHTML = handoff.svg;
  $("handoff-url").textContent = handoff.url;
  $("handoff-meta").textContent =
    formatCount(handoff.file_count, "file") + " · link expires in about an hour";
  $("handoff-backdrop").dataset.url = handoff.url;
  $("handoff-backdrop").classList.remove("hidden");
}

async function loadHandoffs() {
  const items = await callQuiet("list_handoffs");
  const host = $("handoff-list");
  if (!Array.isArray(items) || !items.length) {
    host.innerHTML = "";
    return;
  }
  host.innerHTML = items
    .map(
      (h) => `
      <div class="handoff-row">
        <span class="material-symbols-outlined">link</span>
        <span class="handoff-label">${escapeHtml(h.label)}</span>
        <span class="text-dim">${relativeExpiry(h.expires_ms)}</span>
        <span class="spacer"></span>
        <button class="icon-button" type="button" data-handoff="qr" data-id="${escapeHtml(h.id)}" aria-label="Show code">
          <span class="material-symbols-outlined">qr_code_2</span>
        </button>
        <button class="icon-button" type="button" data-handoff="revoke" data-id="${escapeHtml(h.id)}" aria-label="Stop sharing">
          <span class="material-symbols-outlined">close</span>
        </button>
      </div>`
    )
    .join("");
}

function wireHandoff() {
  $("handoff-pick").addEventListener("click", createHandoff);
  $("handoff-done").addEventListener("click", () =>
    $("handoff-backdrop").classList.add("hidden")
  );
  $("handoff-copy").addEventListener("click", () =>
    copyText($("handoff-backdrop").dataset.url, "Link")
  );
  $("handoff-list").addEventListener("click", async (event) => {
    const button = event.target.closest("[data-handoff]");
    if (!button) return;
    if (button.dataset.handoff === "revoke") {
      await callQuiet("revoke_handoff", { id: button.dataset.id });
      return loadHandoffs();
    }
    // Regenerate the QR on demand rather than holding one per link in memory.
    const items = await callQuiet("list_handoffs");
    const found = (items || []).find((h) => h.id === button.dataset.id);
    if (!found) return loadHandoffs();
    const svg = await callQuiet("get_qr_for_url", { url: found.url, size: 320 });
    showHandoff({ ...found, svg: svg || "" });
  });
}

function wireDevices() {
  wireHandoff();
  $("nearby-list").addEventListener("click", onDeviceAction);
  $("paired-list").addEventListener("click", onDeviceAction);
  $("transfers-list").addEventListener("click", onDeviceAction);

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

  $("self-discoverable").addEventListener("change", async (event) => {
    const restarted = await callQuiet("set_discoverable", { enabled: event.target.checked });
    if (restarted) showToast("Server restarted to start announcing", "info");
    loadIdentity();
    refreshDevices();
  });

  $("self-receive-pick").addEventListener("click", async () => {
    const picked = await callQuiet("pick_receive_folder");
    if (picked) {
      showToast("Received files will go to " + picked, "success");
      loadIdentity();
    }
  });

  $("transfers-clear").addEventListener("click", () => {
    state.transfers.items = state.transfers.items.filter((t) => t.status === "active");
    renderTransfers();
  });

  // --- pairing dialogs ---
  $("pair-out-cancel").addEventListener("click", cancelPairing);
  $("pair-in-accept").addEventListener("click", () => answerIncomingPair(true));
  $("pair-in-reject").addEventListener("click", () => answerIncomingPair(false));

  // --- offers ---
  $("offer-accept").addEventListener("click", () => answerOffer(true));
  $("offer-decline").addEventListener("click", () => answerOffer(false));

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

  // --- browse ---
  $("browse-close").addEventListener("click", () => $("browse-backdrop").classList.add("hidden"));
  $("browse-up").addEventListener("click", () => {
    if (state.browse.path) {
      const idx = state.browse.path.lastIndexOf("/");
      state.browse.path = idx < 0 ? "" : state.browse.path.slice(0, idx);
    } else {
      state.browse.shareId = null;
    }
    loadBrowse();
  });
  $("browse-list").addEventListener("click", (event) => {
    const row = event.target.closest("[data-browse]");
    if (!row || row.dataset.browse !== "open") return;
    if (!state.browse.shareId) {
      state.browse.shareId = row.dataset.target;
      state.browse.path = "";
    } else {
      state.browse.path = state.browse.path
        ? state.browse.path + "/" + row.dataset.target
        : row.dataset.target;
    }
    loadBrowse();
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
    if (!$("offer-backdrop").classList.contains("hidden")) {
      $("offer-backdrop").classList.add("hidden");
      state.offers.shownId = null;
    }
    if (!$("addip-backdrop").classList.contains("hidden")) {
      $("addip-backdrop").classList.add("hidden");
    }
    if (!$("browse-backdrop").classList.contains("hidden")) {
      $("browse-backdrop").classList.add("hidden");
    }
  });
}
