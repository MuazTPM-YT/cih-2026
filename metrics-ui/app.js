/* =====================================================
   Link Console — field ⟶ hospital monitor + live link simulator.

   The sliders POST to the netsim proxy's control API, so moving them
   changes the REAL packet loss / corruption / bandwidth the field
   client is transmitting over — and the stats below react. Data comes
   from the field agent (/api/status) and the gateway (/api/queue,
   /api/observations); if a source is dark it falls back to demo data.

   No build step, no libraries: the one chart is inline SVG.
   ===================================================== */

"use strict";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------
const POLL_MS = 2000;
const FETCH_TIMEOUT_MS = 1500;
const HISTORY = 60;
const SLIDER_DEBOUNCE_MS = 90;

const DEFAULTS = { fieldUrl: "http://localhost:8091", gwUrl: "http://localhost:8080", simUrl: "http://localhost:8088" };
const CFG_KEY = "tgw-console-sources-v2";

const TRIAGE = [
  { key: "critical", label: "Critical", emoji: "🔴", color: "var(--triage-critical)" },
  { key: "high", label: "High", emoji: "🟠", color: "var(--triage-high)" },
  { key: "moderate", label: "Moderate", emoji: "🟡", color: "var(--triage-moderate)" },
  { key: "stable", label: "Stable", emoji: "🟢", color: "var(--triage-stable)" },
];

// ---------------------------------------------------------------------------
// Pure metric functions (unit-checkable — see runSelfTest / ?selftest)
// ---------------------------------------------------------------------------
function tallyFieldQueue(queue) {
  const counts = { queued: 0, sending: 0, delivered: 0, stuck: 0 };
  let retries = 0;
  for (const row of queue || []) {
    const state = String(row.state || "").toLowerCase();
    if (state in counts) counts[state] += 1;
    else if (state === "transmitting" || state === "preparing") counts.sending += 1;
    retries += Number(row.retries) || 0;
  }
  return { counts, retries };
}

function tallyGatewayQueue(gwQueue) {
  const counts = { queued: 0, sending: 0, delivered: 0, stuck: 0 };
  for (const row of gwQueue || []) {
    const state = String(row.state || "").toLowerCase();
    if (state === "receiving") counts.sending += 1;
    else if (state === "complete" || state === "receipt_sent") counts.delivered += 1;
  }
  return { counts };
}

function computeClinical(cases, gwObs) {
  const buckets = { critical: 0, high: 0, moderate: 0, stable: 0 };
  const withPriority = (cases || []).filter((c) => c && c.assessment && c.assessment.priority);
  if (withPriority.length > 0) {
    for (const c of withPriority) if (c.assessment.priority in buckets) buckets[c.assessment.priority] += 1;
    return { buckets, total: withPriority.length, source: "triage" };
  }
  if (Array.isArray(gwObs) && gwObs.length > 0) {
    for (const row of gwObs) {
      const n = Array.isArray(row.flags) ? row.flags.length : 0;
      buckets[n >= 3 ? "critical" : n === 2 ? "high" : n === 1 ? "moderate" : "stable"] += 1;
    }
    return { buckets, total: gwObs.length, source: "flags" };
  }
  return { buckets, total: 0, source: "none" };
}

function computeMetrics(sources) {
  const field = tallyFieldQueue(sources.fieldStatus && sources.fieldStatus.queue);
  const gw = tallyGatewayQueue(sources.gwQueue);
  const haveField = !!(sources.fieldStatus && sources.fieldStatus.queue);
  const counts = haveField ? field.counts : gw.counts;

  const obsCount = Array.isArray(sources.gwObs) ? sources.gwObs.length : null;
  const delivered = obsCount != null ? Math.max(obsCount, counts.delivered) : counts.delivered;
  const attempted = delivered + counts.stuck;
  const successRate = attempted > 0 ? delivered / attempted : null;

  const clinical = computeClinical(sources.cases, sources.gwObs);
  return {
    transport: { queued: counts.queued, sending: counts.sending, delivered, stuck: counts.stuck, attempted, retries: field.retries, successRate },
    clinical,
    deliveredTotal: delivered,
  };
}

function shortId(id) { return String(id || "").slice(0, 8); }

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
let cfg = loadConfig();
const history = [];
let sourceStatus = { field: "offline", gw: "offline", sim: "offline" };
let demoActive = false;
let lastMetrics = null;

function loadConfig() {
  try { const raw = localStorage.getItem(CFG_KEY); if (raw) return { ...DEFAULTS, ...JSON.parse(raw) }; } catch (_) {}
  return { ...DEFAULTS };
}
function saveConfig(next) {
  cfg = { ...cfg, ...next };
  try { localStorage.setItem(CFG_KEY, JSON.stringify(cfg)); } catch (_) {}
}
const $ = (id) => document.getElementById(id);
const base = (u) => String(u || "").replace(/\/+$/, "");

// ---------------------------------------------------------------------------
// Fetch helpers
// ---------------------------------------------------------------------------
async function fetchJSON(url, opts) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), FETCH_TIMEOUT_MS);
  try {
    const res = await fetch(url, { cache: "no-store", signal: ctrl.signal, ...opts });
    if (!res.ok) throw new Error("HTTP " + res.status);
    return await res.json();
  } finally { clearTimeout(timer); }
}

// ---------------------------------------------------------------------------
// Simulation controls — POST to the netsim proxy
// ---------------------------------------------------------------------------
let simPostTimer = null;
let lastSimSnapshot = null;

function readSliders() {
  return {
    lossPct: Number($("in-loss").value),
    corruptPct: Number($("in-corrupt").value),
    kbps: Number($("in-rate").value),
  };
}

function paintSliderLabels(s) {
  $("out-loss").textContent = s.lossPct + "%";
  $("out-corrupt").textContent = s.corruptPct + "%";
  $("out-rate").textContent = s.kbps + " kbps";
  $("slider-loss").classList.toggle("hot", s.lossPct > 0);
  $("slider-corrupt").classList.toggle("hot", s.corruptPct > 0);
}

function scheduleSimPost() {
  if (simPostTimer) clearTimeout(simPostTimer);
  simPostTimer = setTimeout(postSim, SLIDER_DEBOUNCE_MS);
}

async function postSim() {
  const s = readSliders();
  const body = { loss: s.lossPct / 100, corrupt: s.corruptPct / 100, rate_bps: s.kbps * 1000 };
  try {
    const applied = await fetchJSON(base(cfg.simUrl) + "/api/link", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    sourceStatus.sim = "live";
    lastSimSnapshot = applied;
    $("sim-note").classList.remove("warn");
    $("sim-note").textContent =
      "Live link: " + Math.round(applied.loss * 100) + "% loss · " +
      Math.round(applied.corrupt * 100) + "% corruption · " + Math.round(applied.rate_bps / 1000) + " kbps";
  } catch (_) {
    sourceStatus.sim = "offline";
    $("sim-note").classList.add("warn");
    $("sim-note").textContent = "Netsim not reachable at " + cfg.simUrl + " — start it with --control-http, or set its URL under Sources. Sliders take effect once it's up.";
  }
  paintSourceDots();
}

// Reflect the netsim's current state into the sliders (e.g. on load, or if changed elsewhere).
async function syncSlidersFromSim() {
  try {
    const st = await fetchJSON(base(cfg.simUrl) + "/api/link");
    sourceStatus.sim = "live";
    lastSimSnapshot = st;
    $("in-loss").value = Math.round(st.loss * 100);
    $("in-corrupt").value = Math.round(st.corrupt * 100);
    $("in-rate").value = Math.round(st.rate_bps / 1000);
    paintSliderLabels(readSliders());
    $("sim-note").classList.remove("warn");
    $("sim-note").textContent =
      "Live link: " + Math.round(st.loss * 100) + "% loss · " +
      Math.round(st.corrupt * 100) + "% corruption · " + Math.round(st.rate_bps / 1000) + " kbps";
  } catch (_) {
    sourceStatus.sim = "offline";
  }
  paintSourceDots();
}

// ---------------------------------------------------------------------------
// Poll
// ---------------------------------------------------------------------------
async function poll() {
  const [fieldStatus, gwQueue, gwObs] = await Promise.all([
    fetchJSON(base(cfg.fieldUrl) + "/api/status").then((d) => ((sourceStatus.field = "live"), d)).catch(() => (sourceStatus.field = "offline", null)),
    fetchJSON(base(cfg.gwUrl) + "/api/queue").then((d) => ((sourceStatus.gw = "live"), d)).catch(() => null),
    fetchJSON(base(cfg.gwUrl) + "/api/observations").catch(() => null),
  ]);
  if (gwQueue == null && gwObs == null) sourceStatus.gw = "offline";

  const cases = readStoreCases();
  let sources = { fieldStatus, gwQueue, gwObs, cases };

  demoActive = fieldStatus == null && gwQueue == null && gwObs == null && cases.length === 0;
  if (demoActive) sources = mockSources();

  const metrics = computeMetrics(sources);
  lastMetrics = metrics;
  history.push({ t: Date.now(), delivered: metrics.deliveredTotal });
  while (history.length > HISTORY) history.shift();
  render(metrics);
}

function readStoreCases() {
  try { if (typeof TgwStore !== "undefined" && TgwStore.loadCases) return TgwStore.loadCases(); } catch (_) {}
  return [];
}

// ---------------------------------------------------------------------------
// Demo data — only when every data source is dark (sliders still drive netsim)
// ---------------------------------------------------------------------------
let mockT = 0;
const mockState = { delivered: 14, stuck: 0 };
function mockSources() {
  mockT += 1;
  const lossPct = readSliders().lossPct; // let the demo react to the loss slider too
  const goodChance = Math.max(0.05, 0.9 - lossPct / 130);
  if (Math.random() < goodChance) mockState.delivered += 1;
  if (Math.random() < lossPct / 900) mockState.stuck += 1;
  const sending = Math.max(0, Math.round((lossPct / 100) * 4));
  const retries = Math.round((lossPct / 100) * 12);
  const queue = [];
  for (let i = 0; i < mockState.delivered; i++) queue.push({ state: "delivered", retries: 0 });
  for (let i = 0; i < sending; i++) queue.push({ state: "sending", retries: Math.round(retries / Math.max(1, sending)) });
  for (let i = 0; i < mockState.stuck; i++) queue.push({ state: "stuck", retries: 8 });
  const priorities = ["critical", "high", "moderate", "stable"];
  const dist = [1, 3, 5, Math.max(1, mockState.delivered - 9)];
  const cases = [];
  priorities.forEach((p, i) => { for (let k = 0; k < dist[i]; k++) cases.push({ assessment: { priority: p } }); });
  return { fieldStatus: { queue }, gwQueue: [], gwObs: Array.from({ length: mockState.delivered }, () => ({ flags: [] })), cases };
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------
function render(m) {
  const t = m.transport;
  setStat("rate", t.successRate == null ? "—" : Math.round(t.successRate * 100) + "%");
  setStat("delivered", t.delivered);
  setStat("stuck", t.stuck);
  setStat("retries", t.retries);
  document.querySelector('[data-metric="stuck"]').classList.toggle("on", t.stuck > 0);

  renderChart($("chart-delivery"), history);
  renderTriage(m.clinical);
  paintSourceDots();
  renderFooter(m);
}

function setStat(metric, value) {
  const el = document.querySelector('[data-metric="' + metric + '"]');
  if (el && el.textContent !== String(value)) el.textContent = value;
}

function paintSourceDots() {
  paintDot("src-field", sourceStatus.field);
  paintDot("src-gw", sourceStatus.gw);
  paintDot("src-sim", sourceStatus.sim);
}
function paintDot(id, status) {
  const el = $(id);
  if (!el) return;
  el.classList.remove("is-live", "is-demo");
  if (status === "live") el.classList.add("is-live");
  else if (status === "demo") el.classList.add("is-demo");
}

function renderTriage(clinical) {
  const host = $("triage");
  $("triage-total").textContent = clinical.total + " total";
  const max = Math.max(1, ...TRIAGE.map((s) => clinical.buckets[s.key]));
  host.innerHTML = "";
  for (const seg of TRIAGE) {
    const count = clinical.buckets[seg.key];
    const pct = Math.round((count / max) * 100);
    const row = document.createElement("div");
    row.className = "triage-row";
    row.innerHTML =
      '<span class="triage-name"><span class="triage-swatch" style="background:' + seg.color + '"></span>' + seg.emoji + " " + seg.label + "</span>" +
      '<div class="triage-track"><div class="triage-fill" style="width:' + pct + "%;background:" + seg.color + '"></div></div>' +
      '<span class="triage-count">' + count + "</span>";
    host.appendChild(row);
  }
}

function renderFooter(m) {
  const parts = ["field " + sourceStatus.field, "hospital " + sourceStatus.gw, "sim " + sourceStatus.sim];
  let html = parts.join("  ·  ");
  if (demoActive) html += '  ·  <span class="demo">demo data (no live source)</span>';
  $("foot").innerHTML = html;
}

// --- Chart: delivered over time (area + line + hover) ---
const SVGNS = "http://www.w3.org/2000/svg";
function renderChart(host, hist) {
  host.innerHTML = "";
  if (hist.length < 2) { host.innerHTML = '<div class="chart-empty">Collecting data…</div>'; return; }
  const W = host.clientWidth || 760, H = host.clientHeight || 180;
  const pad = { l: 34, r: 12, t: 12, b: 18 };
  const svg = document.createElementNS(SVGNS, "svg");
  svg.setAttribute("viewBox", "0 0 " + W + " " + H);
  const defs = document.createElementNS(SVGNS, "defs");
  defs.innerHTML = '<linearGradient id="tealfill" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="var(--teal)" stop-opacity="0.28"/><stop offset="100%" stop-color="var(--teal)" stop-opacity="0.02"/></linearGradient>';
  svg.appendChild(defs);

  const maxY = Math.max(1, ...hist.map((d) => d.delivered));
  const x0 = pad.l, x1 = W - pad.r, y0 = H - pad.b, y1 = pad.t;
  const sx = (i) => x0 + (i / (hist.length - 1)) * (x1 - x0);
  const sy = (v) => y0 - (v / maxY) * (y0 - y1);

  for (let g = 0; g <= 2; g++) {
    const v = Math.round((maxY / 2) * g), yy = sy(v);
    const l = document.createElementNS(SVGNS, "line");
    l.setAttribute("class", "grid-line"); l.setAttribute("x1", x0); l.setAttribute("x2", x1); l.setAttribute("y1", yy); l.setAttribute("y2", yy);
    svg.appendChild(l);
    const tx = document.createElementNS(SVGNS, "text");
    tx.setAttribute("class", "axis-text"); tx.setAttribute("x", 4); tx.setAttribute("y", yy + 3); tx.textContent = v;
    svg.appendChild(tx);
  }

  let line = "";
  hist.forEach((d, i) => { line += (i === 0 ? "M" : "L") + sx(i).toFixed(1) + " " + sy(d.delivered).toFixed(1) + " "; });
  const area = line + "L" + sx(hist.length - 1).toFixed(1) + " " + y0 + " L" + x0 + " " + y0 + " Z";
  const a = document.createElementNS(SVGNS, "path"); a.setAttribute("class", "series-area"); a.setAttribute("d", area); svg.appendChild(a);
  const p = document.createElementNS(SVGNS, "path"); p.setAttribute("class", "series-line"); p.setAttribute("d", line); svg.appendChild(p);

  const cross = document.createElementNS(SVGNS, "line"); cross.setAttribute("class", "crosshair"); cross.setAttribute("y1", y1); cross.setAttribute("y2", y0); cross.style.display = "none"; svg.appendChild(cross);
  const dot = document.createElementNS(SVGNS, "circle"); dot.setAttribute("class", "cursor-dot"); dot.setAttribute("r", 4); dot.style.display = "none"; svg.appendChild(dot);
  const overlay = document.createElementNS(SVGNS, "rect");
  overlay.setAttribute("x", x0); overlay.setAttribute("y", y1); overlay.setAttribute("width", Math.max(0, x1 - x0)); overlay.setAttribute("height", Math.max(0, y0 - y1)); overlay.setAttribute("fill", "transparent");
  overlay.addEventListener("mousemove", (e) => {
    const rect = svg.getBoundingClientRect();
    const px = (e.clientX - rect.left) * (W / rect.width);
    const i = Math.max(0, Math.min(hist.length - 1, Math.round(((px - x0) / Math.max(1, x1 - x0)) * (hist.length - 1))));
    const X = sx(i), Y = sy(hist[i].delivered);
    cross.style.display = ""; cross.setAttribute("x1", X); cross.setAttribute("x2", X);
    dot.style.display = ""; dot.setAttribute("cx", X); dot.setAttribute("cy", Y);
    showTip(e.clientX, rect.top + (Y / H) * rect.height, '<span class="tt-key">delivered</span> ' + hist[i].delivered);
  });
  overlay.addEventListener("mouseleave", () => { cross.style.display = "none"; dot.style.display = "none"; hideTip(); });
  svg.appendChild(overlay);
  host.appendChild(svg);
}

// --- Tooltip ---
let tipEl = null;
function showTip(x, y, html) {
  if (!tipEl) { tipEl = document.createElement("div"); tipEl.className = "tt"; document.body.appendChild(tipEl); }
  tipEl.innerHTML = html; tipEl.hidden = false; tipEl.style.left = x + "px"; tipEl.style.top = y + "px";
}
function hideTip() { if (tipEl) tipEl.hidden = true; }

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------
function wireSliders() {
  ["in-loss", "in-corrupt", "in-rate"].forEach((id) => {
    $(id).addEventListener("input", () => { paintSliderLabels(readSliders()); scheduleSimPost(); });
  });
  document.querySelectorAll(".presets button").forEach((b) => {
    b.addEventListener("click", () => {
      $("in-loss").value = b.dataset.loss;
      $("in-corrupt").value = b.dataset.corrupt;
      $("in-rate").value = b.dataset.rate;
      paintSliderLabels(readSliders());
      postSim();
    });
  });
}

function wireDrawer() {
  const open = () => { $("url-field").value = cfg.fieldUrl; $("url-gw").value = cfg.gwUrl; $("url-sim").value = cfg.simUrl; $("drawer").hidden = false; $("scrim").hidden = false; };
  const close = () => { $("drawer").hidden = true; $("scrim").hidden = true; };
  $("btn-settings").addEventListener("click", open);
  $("drawer-cancel").addEventListener("click", close);
  $("scrim").addEventListener("click", close);
  $("drawer-save").addEventListener("click", () => {
    saveConfig({ fieldUrl: $("url-field").value.trim() || DEFAULTS.fieldUrl, gwUrl: $("url-gw").value.trim() || DEFAULTS.gwUrl, simUrl: $("url-sim").value.trim() || DEFAULTS.simUrl });
    close(); syncSlidersFromSim(); poll().catch(() => {});
  });
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") close(); });
}

// ---------------------------------------------------------------------------
// Self-test
// ---------------------------------------------------------------------------
function runSelfTest() {
  const r = [];
  const ok = (name, cond) => r.push({ name, pass: !!cond });
  const m1 = computeMetrics({ fieldStatus: { queue: [{ state: "delivered", retries: 0 }, { state: "delivered", retries: 2 }, { state: "sending", retries: 1 }, { state: "queued", retries: 0 }, { state: "stuck", retries: 8 }] }, gwQueue: [], gwObs: null, cases: [] });
  ok("counts", m1.transport.delivered === 2 && m1.transport.sending === 1 && m1.transport.stuck === 1);
  ok("retries", m1.transport.retries === 11);
  ok("rate", Math.abs(m1.transport.successRate - 2 / 3) < 1e-9);
  const m2 = computeMetrics({ fieldStatus: null, gwQueue: [{ state: "complete" }], gwObs: [{ flags: [] }], cases: [] });
  ok("delivered prefers observations", m2.transport.delivered === 1);
  const c1 = computeClinical([{ assessment: { priority: "critical" } }], null);
  ok("clinical triage", c1.buckets.critical === 1 && c1.source === "triage");
  const c2 = computeClinical([], [{ flags: [] }, { flags: ["a", "b", "c"] }]);
  ok("clinical flags proxy", c2.buckets.stable === 1 && c2.buckets.critical === 1);
  const passed = r.filter((x) => x.pass).length;
  console.table(r); console.log("[selftest] " + passed + "/" + r.length + " passed");
  return passed === r.length;
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------
function boot() {
  wireSliders();
  wireDrawer();
  paintSliderLabels(readSliders());
  syncSlidersFromSim();          // adopt the netsim's current link if it's up
  poll().catch(() => {});
  setInterval(() => poll().catch(() => {}), POLL_MS);
  if (new URLSearchParams(location.search).has("selftest")) runSelfTest();
}

if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot);
else boot();
