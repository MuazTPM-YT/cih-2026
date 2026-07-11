/* =====================================================
   TGW Clinical Dashboard — app.js
   Vanilla JS: fetch + setInterval polling, no libraries
   ===================================================== */

// ─── Configuration ───────────────────────────────────
// Single setting to switch mock ↔ real API.
// Mock mode:  ""  (empty string — loads from relative mock/ path)
// Real mode:  "http://<gateway>:8080"  (set when Twaha confirms the API is live)
const API_BASE = "";

// ─── URL Helper ──────────────────────────────────────
// Encapsulates mock-vs-real URL differences.
// In mock mode: observations → mock/observations.json, queue → mock/queue.json
// In real mode: observations → <API_BASE>/api/observations, etc.
function apiUrl(endpoint) {
  if (API_BASE === "") {
    // Mock mode — serve static fixture files
    const mockMap = {
      observations: "mock/observations.json",
      queue:        "mock/queue.json",
    };
    return mockMap[endpoint] || endpoint;
  }
  // Real mode
  const realMap = {
    observations: "/api/observations",
    queue:        "/api/queue",
  };
  return API_BASE + (realMap[endpoint] || endpoint);
}

// Resolve an image URL — in mock mode, images won't exist so return null.
// In real mode, return the full URL.
function imageUrl(path) {
  if (!path) return null;
  if (API_BASE === "") return null; // no image endpoint in mock mode
  return API_BASE + path;
}

// ─── State ───────────────────────────────────────────
let observations    = [];      // last successful observations
let queue           = [];      // last successful queue
let knownBundleIds  = new Set();  // for new-arrival detection
let firstLoad       = true;
let pollsOk         = 0;
let pollsFail       = 0;
let isReconnecting  = false;
let pollInFlight    = false;   // guard against overlapping polls
const startTime     = Date.now();

const POLL_INTERVAL = 2000;    // ms

// ─── DOM References ──────────────────────────────────
const $obsFeed       = document.getElementById("observations-feed");
const $xferFeed      = document.getElementById("transfer-feed");
const $obsCount      = document.getElementById("obs-count");
const $xferCount     = document.getElementById("xfer-count");
const $connStatus    = document.getElementById("connection-status");
const $statusText    = document.querySelector("#connection-status .status-text");
const $reconnBanner  = document.getElementById("reconnect-banner");
const $lastUpdated   = document.getElementById("last-updated");
const $modeValue     = document.getElementById("mode-value");
const $pollsOk       = document.getElementById("polls-ok-value");
const $pollsFail     = document.getElementById("polls-fail-value");
const $endpoint      = document.getElementById("endpoint-value");
const $uptime        = document.getElementById("uptime-value");

// ─── Initialization ──────────────────────────────────
function init() {
  // Set mode display
  $modeValue.textContent = API_BASE === "" ? "Mock" : "Live";
  $endpoint.textContent  = API_BASE === "" ? "mock/*.json" : API_BASE;

  // Start polling
  poll();
  setInterval(pollGuard, POLL_INTERVAL);

  // Update uptime counter every second
  setInterval(updateUptime, 1000);
}

function updateUptime() {
  const s = Math.floor((Date.now() - startTime) / 1000);
  const m = Math.floor(s / 60);
  const h = Math.floor(m / 60);
  if (h > 0) {
    $uptime.textContent = `${h}h ${m % 60}m`;
  } else if (m > 0) {
    $uptime.textContent = `${m}m ${s % 60}s`;
  } else {
    $uptime.textContent = `${s}s`;
  }
}

// ─── Poll Guard (prevents overlapping polls) ─────────
function pollGuard() {
  if (!pollInFlight) {
    poll();
  }
}

// ─── Polling ─────────────────────────────────────────
async function poll() {
  pollInFlight = true;
  try {
    const [obsRes, queueRes] = await Promise.all([
      fetch(apiUrl("observations")),
      fetch(apiUrl("queue")),
    ]);

    if (!obsRes.ok || !queueRes.ok) {
      throw new Error(`HTTP ${obsRes.status}/${queueRes.status}`);
    }

    const newObs   = await obsRes.json();
    const newQueue = await queueRes.json();

    // Detect newly arrived bundle IDs (skip on first load to avoid animating everything)
    const newBundleIds = new Set();
    if (!firstLoad) {
      for (const o of newObs) {
        if (!knownBundleIds.has(o.bundle_id)) newBundleIds.add(o.bundle_id);
      }
      for (const q of newQueue) {
        if (!knownBundleIds.has(q.bundle_id)) newBundleIds.add(q.bundle_id);
      }
    }

    // Update known set
    for (const o of newObs) knownBundleIds.add(o.bundle_id);
    for (const q of newQueue) knownBundleIds.add(q.bundle_id);

    observations = newObs;
    queue        = newQueue;
    firstLoad    = false;

    // Render
    renderObservations(newBundleIds);
    renderTransfers(newBundleIds);
    updateCounts();

    // Success state
    pollsOk++;
    setConnected(true);
    $lastUpdated.textContent = "Updated " + formatTime(new Date());

  } catch (err) {
    pollsFail++;
    setConnected(false);
    console.warn("[TGW] Poll failed:", err.message);
    // Keep last good data on screen — do NOT clear observations/queue
  } finally {
    pollInFlight = false;
    $pollsOk.textContent   = pollsOk;
    $pollsFail.textContent = pollsFail;
  }
}

// ─── Connection State ────────────────────────────────
function setConnected(ok) {
  if (ok) {
    isReconnecting = false;
    $connStatus.classList.remove("status-err");
    $connStatus.classList.add("status-ok");
    $statusText.textContent = "Connected";
    $reconnBanner.hidden = true;
  } else {
    isReconnecting = true;
    $connStatus.classList.remove("status-ok");
    $connStatus.classList.add("status-err");
    $statusText.textContent = "Reconnecting…";
    $reconnBanner.hidden = false;
  }
}

// ─── Render Observations ─────────────────────────────
function renderObservations(newBundleIds) {
  if (observations.length === 0) {
    $obsFeed.innerHTML = '<p class="feed-empty">Waiting for clinical data…</p>';
    return;
  }

  const fragment = document.createDocumentFragment();

  for (const obs of observations) {
    const card = document.createElement("article");
    card.className = "obs-card";
    card.setAttribute("role", "article");
    card.id = "obs-" + obs.bundle_id;

    if (newBundleIds.has(obs.bundle_id)) {
      card.classList.add("obs-new");
      // Remove highlight class after animation
      setTimeout(() => card.classList.remove("obs-new"), 1200);
    }

    if (obs.kind === "vitals") {
      card.innerHTML = renderVitalsCard(obs);
    } else if (obs.kind === "image") {
      card.innerHTML = renderImageCard(obs);
    }

    fragment.appendChild(card);
  }

  $obsFeed.innerHTML = "";
  $obsFeed.appendChild(fragment);

  // Attach FHIR toggle event listeners
  $obsFeed.querySelectorAll(".fhir-toggle").forEach(btn => {
    btn.addEventListener("click", handleFhirToggle);
    btn.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        handleFhirToggle.call(btn);
      }
    });
  });
}

function renderVitalsCard(obs) {
  const fhirId = "fhir-" + obs.bundle_id;
  return `
    <div class="obs-card-header">
      <span class="obs-patient">
        <span class="obs-kind-badge kind-vitals">Vitals</span>
        ${escHtml(obs.patient_id)}
      </span>
      <span class="obs-time">${formatTimestamp(obs.received_at)}</span>
    </div>
    <div class="obs-summary">${escHtml(obs.summary)}</div>
    <button class="fhir-toggle" aria-expanded="false" aria-controls="${fhirId}"
            tabindex="0" type="button">
      <span class="fhir-chevron" aria-hidden="true">▶</span>
      View FHIR R5 JSON
    </button>
    <div class="fhir-content" id="${fhirId}" hidden>
      <pre>${escHtml(JSON.stringify(obs.fhir, null, 2))}</pre>
    </div>
  `;
}

function renderImageCard(obs) {
  const imgSrc = imageUrl(obs.image_url);
  const altText = `Clinical image for patient ${obs.patient_id} received ${formatTimestamp(obs.received_at)}`;

  return `
    <div class="obs-card-header">
      <span class="obs-patient">
        <span class="obs-kind-badge kind-image">Image</span>
        ${escHtml(obs.patient_id)}
      </span>
      <span class="obs-time">${formatTimestamp(obs.received_at)}</span>
    </div>
    <div class="obs-image-container">
      ${imgSrc
        ? `<img class="obs-image" src="${escAttr(imgSrc)}" alt="${escAttr(altText)}" loading="lazy">`
        : `<div class="obs-image-placeholder">Image available when gateway is live</div>`
      }
    </div>
  `;
}

// ─── FHIR Toggle Handler ─────────────────────────────
function handleFhirToggle() {
  const targetId = this.getAttribute("aria-controls");
  const target   = document.getElementById(targetId);
  const expanded = this.getAttribute("aria-expanded") === "true";

  this.setAttribute("aria-expanded", !expanded);
  target.hidden = expanded;
}

// ─── Render Transfers ────────────────────────────────
function renderTransfers(newBundleIds) {
  if (queue.length === 0) {
    $xferFeed.innerHTML = '<p class="feed-empty">No transfers yet…</p>';
    return;
  }

  const fragment = document.createDocumentFragment();

  for (const q of queue) {
    const card = document.createElement("article");
    card.className = "xfer-card";
    card.setAttribute("role", "article");
    card.id = "xfer-" + q.bundle_id;

    if (newBundleIds.has(q.bundle_id)) {
      card.classList.add("xfer-new");
      setTimeout(() => card.classList.remove("xfer-new"), 1200);
    }

    card.innerHTML = renderTransferCard(q);
    fragment.appendChild(card);
  }

  $xferFeed.innerHTML = "";
  $xferFeed.appendChild(fragment);
}

function renderTransferCard(q) {
  const stateClass = stateToClass(q.state);
  const stateLabel = stateToLabel(q.state);
  const stateIcon  = stateToIcon(q.state);

  // Progress calculation — symbols_received can exceed symbols_needed (repair symbols)
  const pct = q.symbols_needed > 0
    ? Math.min(100, Math.round((q.symbols_received / q.symbols_needed) * 100))
    : 100;

  const progressLabel = q.state === "receiving"
    ? `receiving ${q.symbols_received}/${q.symbols_needed} symbols`
    : `${q.symbols_received}/${q.symbols_needed} symbols`;

  return `
    <div class="xfer-card-header">
      <span class="xfer-bundle-id" title="${escAttr(q.bundle_id)}">${escHtml(q.bundle_id.substring(0, 18))}…</span>
      <span class="xfer-state-badge ${stateClass}">
        ${stateIcon} ${stateLabel}
      </span>
    </div>
    <div class="xfer-progress ${stateClass}">
      <div class="xfer-progress-bar-bg">
        <div class="xfer-progress-bar" style="width: ${pct}%"></div>
      </div>
      <div class="xfer-progress-label">
        <span class="xfer-progress-symbols">${escHtml(progressLabel)}</span>
        <span>${pct}%</span>
      </div>
    </div>
    <div class="xfer-timestamps">
      <span>First seen: ${formatTimestamp(q.first_seen)}</span>
      <span>${q.completed_at ? "Completed: " + formatTimestamp(q.completed_at) : "In progress"}</span>
    </div>
  `;
}

// ─── Transfer State Helpers ──────────────────────────
function stateToClass(state) {
  switch (state) {
    case "receiving":    return "state-receiving";
    case "complete":     return "state-complete";
    case "receipt_sent": return "state-receipt-sent";
    default:             return "";
  }
}

function stateToLabel(state) {
  switch (state) {
    case "receiving":    return "Receiving";
    case "complete":     return "Complete";
    case "receipt_sent": return "Receipt Sent ✓";
    default:             return state;
  }
}

function stateToIcon(state) {
  switch (state) {
    case "receiving":    return "◌";
    case "complete":     return "●";
    case "receipt_sent": return "✓";
    default:             return "";
  }
}

// ─── Update Counts ───────────────────────────────────
function updateCounts() {
  $obsCount.textContent  = observations.length;
  $xferCount.textContent = queue.length;
}

// ─── Utility Functions ───────────────────────────────
function formatTimestamp(iso) {
  if (!iso) return "—";
  try {
    const d = new Date(iso);
    return d.toLocaleTimeString("en-GB", {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    }) + " · " + d.toLocaleDateString("en-GB", {
      day: "2-digit",
      month: "short",
    });
  } catch {
    return iso;
  }
}

function formatTime(d) {
  return d.toLocaleTimeString("en-GB", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

function escHtml(str) {
  if (str == null) return "";
  return String(str)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function escAttr(str) {
  return escHtml(str).replace(/'/g, "&#39;");
}

// ─── Start ───────────────────────────────────────────
init();
