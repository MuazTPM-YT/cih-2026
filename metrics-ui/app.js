/* =====================================================
   Telemetry Console — field ⟶ hospital link monitor

   Polls the field agent (/api/status) and the gateway
   (/api/queue, /api/observations) every few seconds, reconciles
   them into one view of the pipeline, and draws it. If a source
   is unreachable it falls back to the shared browser store, then
   to clearly-badged demo data, so the console never looks dead.

   No build step, no external libraries: every chart is inline SVG.
   ===================================================== */

"use strict";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------
const POLL_MS = 2500;
const FETCH_TIMEOUT_MS = 1800;
const HISTORY = 60; // samples retained for the time series / sparkline

const DEFAULTS = { fieldUrl: "http://localhost:8091", gwUrl: "http://localhost:8080" };
const CFG_KEY = "tgw-metrics-sources-v1";

const TRANSPORT_STATES = [
  { key: "queued", color: "var(--st-queued)" },
  { key: "sending", color: "var(--st-sending)" },
  { key: "delivered", color: "var(--st-delivered)" },
  { key: "stuck", color: "var(--st-stuck)" },
];

const TRIAGE = [
  { key: "critical", label: "Critical", emoji: "🔴", color: "var(--triage-critical)" },
  { key: "high", label: "High", emoji: "🟠", color: "var(--triage-high)" },
  { key: "moderate", label: "Moderate", emoji: "🟡", color: "var(--triage-moderate)" },
  { key: "stable", label: "Stable", emoji: "🟢", color: "var(--triage-stable)" },
];

// ---------------------------------------------------------------------------
// Pure metric functions (no DOM — unit-checkable, see runSelfTest)
// ---------------------------------------------------------------------------

/** Tally field-agent queue states into the canonical lifecycle buckets. */
function tallyFieldQueue(queue) {
  const counts = { queued: 0, sending: 0, delivered: 0, stuck: 0 };
  let retries = 0;
  for (const row of queue || []) {
    const state = String(row.state || "").toLowerCase();
    if (state in counts) counts[state] += 1;
    retries += Number(row.retries) || 0;
    // "preparing"/"transmitting" from the bridge map onto sending.
    if (state === "transmitting" || state === "preparing") counts.sending += 1;
  }
  return { counts, retries };
}

/** Derive transport buckets from the gateway queue when the field agent is absent. */
function tallyGatewayQueue(gwQueue) {
  const counts = { queued: 0, sending: 0, delivered: 0, stuck: 0 };
  const inflight = [];
  for (const row of gwQueue || []) {
    const state = String(row.state || "").toLowerCase();
    if (state === "receiving") {
      counts.sending += 1;
      const need = Number(row.symbols_needed) || 0;
      const got = Number(row.symbols_received) || 0;
      const pct = need > 0 ? Math.min(100, Math.round((got / need) * 100)) : 0;
      inflight.push({ id: shortId(row.bundle_id), received: got, needed: need, pct });
    } else if (state === "complete" || state === "receipt_sent") {
      counts.delivered += 1;
    }
  }
  return { counts, inflight };
}

/** Reconcile all sources into one metrics object. */
function computeMetrics(sources) {
  const field = tallyFieldQueue(sources.fieldStatus && sources.fieldStatus.queue);
  const gw = tallyGatewayQueue(sources.gwQueue);

  // Prefer the field agent's lifecycle view; fill gaps from the gateway.
  const haveField = !!(sources.fieldStatus && sources.fieldStatus.queue);
  const counts = haveField ? field.counts : gw.counts;

  // Delivered truth: the gateway's stored observations are what actually
  // survived the link; fall back to the transport count.
  const obsCount = Array.isArray(sources.gwObs) ? sources.gwObs.length : null;
  const delivered = obsCount != null ? Math.max(obsCount, counts.delivered) : counts.delivered;

  const attempted = delivered + counts.stuck;
  const successRate = attempted > 0 ? delivered / attempted : null;

  const clinical = computeClinical(sources.cases, sources.gwObs);

  return {
    transport: {
      queued: counts.queued,
      sending: counts.sending,
      delivered,
      stuck: counts.stuck,
      attempted,
      retries: field.retries,
      successRate,
      inflight: gw.inflight,
    },
    clinical,
    deliveredTotal: delivered,
    clinicalSource: clinical.source,
  };
}

/**
 * Clinical severity distribution. Prefers real triage from the shared field
 * store (assessment.priority); else derives a findings-severity proxy from the
 * gateway's plausibility flags (0 flags → stable … 3+ → critical).
 */
function computeClinical(cases, gwObs) {
  const buckets = { critical: 0, high: 0, moderate: 0, stable: 0 };

  const withPriority = (cases || []).filter((c) => c && c.assessment && c.assessment.priority);
  if (withPriority.length > 0) {
    for (const c of withPriority) {
      const p = c.assessment.priority;
      if (p in buckets) buckets[p] += 1;
    }
    return { buckets, total: withPriority.length, source: "triage" };
  }

  if (Array.isArray(gwObs) && gwObs.length > 0) {
    for (const row of gwObs) {
      const n = Array.isArray(row.flags) ? row.flags.length : 0;
      const key = n >= 3 ? "critical" : n === 2 ? "high" : n === 1 ? "moderate" : "stable";
      buckets[key] += 1;
    }
    return { buckets, total: gwObs.length, source: "flags" };
  }

  return { buckets, total: 0, source: "none" };
}

function shortId(id) {
  return String(id || "").slice(0, 8);
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
let cfg = loadConfig();
const history = []; // [{ t, delivered, retries }]
let sourceStatus = { field: "offline", gw: "offline" }; // live | offline | demo
let lastMetrics = null;
let demoActive = false;
const startedAt = Date.now();

function loadConfig() {
  try {
    const raw = localStorage.getItem(CFG_KEY);
    if (raw) return { ...DEFAULTS, ...JSON.parse(raw) };
  } catch (_) { /* ignore */ }
  return { ...DEFAULTS };
}
function saveConfig(next) {
  cfg = { ...cfg, ...next };
  try { localStorage.setItem(CFG_KEY, JSON.stringify(cfg)); } catch (_) { /* ignore */ }
}

// ---------------------------------------------------------------------------
// Fetch with timeout
// ---------------------------------------------------------------------------
async function fetchJSON(url) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), FETCH_TIMEOUT_MS);
  try {
    const res = await fetch(url, { signal: ctrl.signal, cache: "no-store" });
    if (!res.ok) throw new Error("HTTP " + res.status);
    return await res.json();
  } finally {
    clearTimeout(timer);
  }
}

// ---------------------------------------------------------------------------
// Poll loop
// ---------------------------------------------------------------------------
async function poll() {
  const base = { field: cfg.fieldUrl.replace(/\/+$/, ""), gw: cfg.gwUrl.replace(/\/+$/, "") };

  const [fieldStatus, gwQueue, gwObs] = await Promise.all([
    fetchJSON(base.field + "/api/status").then((d) => (sourceStatus.field = "live", d)).catch(() => null),
    fetchJSON(base.gw + "/api/queue").then((d) => (sourceStatus.gw = "live", d)).catch(() => null),
    fetchJSON(base.gw + "/api/observations").catch(() => null),
  ]);

  if (fieldStatus == null) sourceStatus.field = "offline";
  if (gwQueue == null && gwObs == null) sourceStatus.gw = "offline";

  const cases = readStoreCases();
  let sources = { fieldStatus, gwQueue, gwObs, cases };

  const nothingLive = fieldStatus == null && gwQueue == null && gwObs == null && cases.length === 0;
  demoActive = nothingLive;
  if (nothingLive) {
    sources = mockSources();
    sourceStatus = { field: "demo", gw: "demo" };
  }

  const metrics = computeMetrics(sources);
  lastMetrics = metrics;
  pushHistory(metrics);
  render(metrics);
}

function pushHistory(metrics) {
  history.push({ t: Date.now(), delivered: metrics.deliveredTotal, retries: metrics.transport.retries });
  while (history.length > HISTORY) history.shift();
}

function readStoreCases() {
  try {
    if (typeof TgwStore !== "undefined" && TgwStore.loadCases) return TgwStore.loadCases();
  } catch (_) { /* ignore */ }
  return [];
}

// ---------------------------------------------------------------------------
// Demo data (only when every source is dark; badged clearly)
// ---------------------------------------------------------------------------
let mockT = 0;
const mockState = { delivered: 18, stuck: 1 };
function mockSources() {
  mockT += 1;
  if (Math.random() < 0.7) mockState.delivered += Math.floor(Math.random() * 2) + 1;
  if (Math.random() < 0.06) mockState.stuck += 1;
  const sending = Math.floor(1 + Math.abs(Math.sin(mockT / 3)) * 3);
  const queued = Math.floor(Math.abs(Math.cos(mockT / 5)) * 3);
  const queue = [];
  for (let i = 0; i < mockState.delivered; i++) queue.push({ short_id: "d" + i, kind: "vitals", state: "delivered", retries: Math.random() < 0.3 ? 1 : 0 });
  for (let i = 0; i < sending; i++) queue.push({ short_id: "s" + i, kind: "vitals", state: "sending", retries: Math.floor(Math.random() * 3) });
  for (let i = 0; i < queued; i++) queue.push({ short_id: "q" + i, kind: "image", state: "queued", retries: 0 });
  for (let i = 0; i < mockState.stuck; i++) queue.push({ short_id: "x" + i, kind: "image", state: "stuck", retries: 8 });

  const gwQueue = [];
  for (let i = 0; i < sending; i++) {
    const need = 12 + Math.floor(Math.random() * 8);
    gwQueue.push({ bundle_id: "recv" + i + "aa", state: "receiving", symbols_received: Math.floor(need * (0.3 + Math.random() * 0.6)), symbols_needed: need });
  }
  const priorities = ["critical", "high", "moderate", "stable"];
  const cases = [];
  const dist = [2, 4, 6, mockState.delivered];
  priorities.forEach((p, i) => { for (let k = 0; k < dist[i]; k++) cases.push({ bundleId: p + k, assessment: { priority: p } }); });

  return { fieldStatus: { queue }, gwQueue, gwObs: Array.from({ length: mockState.delivered }, (_, i) => ({ bundle_id: "o" + i, flags: [] })), cases };
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------
const $ = (id) => document.getElementById(id);

function render(m) {
  renderLinkBar(m);
  renderKPIs(m);
  renderTimeSeries($("chart-delivery"), history);
  renderStateBars(m.transport);
  renderSparkline($("spark-retries"), history);
  renderInflight(m.transport.inflight);
  renderDonut(m.clinical);
  renderLegend(m.clinical);
  renderFooter(m);
  renderTable(m);
}

function setKpi(metric, value, tick) {
  const el = document.querySelector('[data-metric="' + metric + '"]');
  if (!el) return;
  if (el.textContent !== String(value)) {
    el.textContent = value;
    if (tick) { el.classList.remove("tick"); void el.offsetWidth; el.classList.add("tick"); }
  }
}

function renderKPIs(m) {
  const t = m.transport;
  setKpi("delivered", t.delivered, true);
  setKpi("inflight", t.queued + t.sending);
  setKpi("rate", t.successRate == null ? "—" : Math.round(t.successRate * 100) + "%", true);
  setKpi("stuck", t.stuck);
  setKpi("critical", m.clinical.buckets.critical);
}

function renderLinkBar(m) {
  const rate = m.transport.successRate;
  $("link-rate").textContent = rate == null ? "— %" : Math.round(rate * 100) + " %";

  applyEndpoint($("ep-field"), $("field-state"), $("field-meta"), sourceStatus.field, metaField(m));
  applyEndpoint($("ep-hospital"), $("gw-state"), $("gw-meta"), sourceStatus.gw, metaGw(m));

  updateWave(rate == null ? 0.5 : rate);
}

function metaField(m) {
  const t = m.transport;
  return t.queued + t.sending + t.delivered + t.stuck + " bundles";
}
function metaGw(m) {
  return m.transport.delivered + " received";
}

function applyEndpoint(root, wordEl, metaEl, status, meta) {
  root.classList.remove("is-live", "is-offline", "is-demo");
  if (status === "live") { root.classList.add("is-live"); wordEl.textContent = "Live"; }
  else if (status === "demo") { root.classList.add("is-demo"); wordEl.textContent = "Demo"; }
  else { root.classList.add("is-offline"); wordEl.textContent = "Offline"; }
  metaEl.textContent = meta;
}

function renderStateBars(t) {
  const host = $("statebars");
  const max = Math.max(1, t.queued, t.sending, t.delivered, t.stuck);
  const total = t.queued + t.sending + t.delivered + t.stuck;
  host.innerHTML = "";
  for (const s of TRANSPORT_STATES) {
    const count = t[s.key];
    const pct = Math.round((count / max) * 100);
    const share = total > 0 ? Math.round((count / total) * 100) : 0;
    const row = document.createElement("div");
    row.className = "statebar";
    row.innerHTML =
      '<span class="statebar-name"><span class="statebar-dot" style="background:' + s.color + '"></span>' + s.key + "</span>" +
      '<div class="statebar-track"><div class="statebar-fill" style="width:' + pct + "%;background:" + s.color + '"></div></div>' +
      '<span class="statebar-count">' + count + "</span>";
    attachTip(row, () => s.key.replace(/^\w/, (c) => c.toUpperCase()) + ": " + count + " · " + share + "% of queue");
    host.appendChild(row);
  }
}

function renderInflight(inflight) {
  const host = $("inflight-bars");
  host.innerHTML = "";
  if (!inflight || inflight.length === 0) {
    host.innerHTML = '<p class="empty-line">No bundle mid-flight.</p>';
    return;
  }
  for (const b of inflight.slice(0, 6)) {
    const row = document.createElement("div");
    row.className = "inflight-row";
    row.innerHTML =
      '<span class="inflight-id">' + b.id + "</span>" +
      '<div class="inflight-track"><div class="inflight-fill" style="width:' + b.pct + '%"></div></div>' +
      '<span class="inflight-pct">' + b.pct + "%</span>";
    attachTip(row, () => "Bundle " + b.id + ": " + b.received + " / " + b.needed + " symbols decoded");
    host.appendChild(row);
  }
}

// --- Donut (SVG arcs, 2px surface gap between segments) ---
const SVGNS = "http://www.w3.org/2000/svg";
function renderDonut(clinical) {
  const wrap = $("donut-wrap");
  wrap.innerHTML = "";
  const size = 168, cx = size / 2, cy = size / 2, r = 66, stroke = 22;
  const svg = document.createElementNS(SVGNS, "svg");
  svg.setAttribute("viewBox", "0 0 " + size + " " + size);

  const total = clinical.total;
  const track = document.createElementNS(SVGNS, "circle");
  track.setAttribute("cx", cx); track.setAttribute("cy", cy); track.setAttribute("r", r);
  track.setAttribute("fill", "none"); track.setAttribute("stroke", "var(--panel-2)"); track.setAttribute("stroke-width", stroke);
  svg.appendChild(track);

  if (total > 0) {
    const circ = 2 * Math.PI * r;
    const gap = 2; // px surface gap between segments
    let cumulative = 0; // length consumed so far, clockwise from the top
    for (const seg of TRIAGE) {
      const val = clinical.buckets[seg.key];
      if (val <= 0) continue;
      const frac = val / total;
      const len = frac * circ;
      const dash = Math.max(0, len - gap);
      const arc = document.createElementNS(SVGNS, "circle");
      arc.setAttribute("class", "donut-seg");
      arc.setAttribute("cx", cx); arc.setAttribute("cy", cy); arc.setAttribute("r", r);
      arc.setAttribute("fill", "none");
      arc.setAttribute("stroke", seg.color);
      arc.setAttribute("stroke-width", stroke);
      arc.setAttribute("stroke-linecap", "butt");
      // A full-circle stroke, shown only for `dash` length, shifted to start at `cumulative`.
      arc.setAttribute("stroke-dasharray", dash + " " + (circ - dash));
      arc.setAttribute("stroke-dashoffset", String(-cumulative));
      arc.setAttribute("transform", "rotate(-90 " + cx + " " + cy + ")");
      const pct = Math.round(frac * 100);
      attachTip(arc, () => seg.emoji + " " + seg.label + ": " + val + " · " + pct + "%");
      arc.addEventListener("mouseenter", () => svg.querySelectorAll(".donut-seg").forEach((s) => { if (s !== arc) s.classList.add("dim"); }));
      arc.addEventListener("mouseleave", () => svg.querySelectorAll(".donut-seg").forEach((s) => s.classList.remove("dim")));
      svg.appendChild(arc);
      cumulative += len;
    }
  }
  wrap.appendChild(svg);

  const center = document.createElement("div");
  center.className = "donut-center";
  center.innerHTML = '<span class="donut-total">' + total + '</span><span class="donut-total-label">patients</span>';
  wrap.appendChild(center);
}

function renderLegend(clinical) {
  const host = $("triage-legend");
  host.innerHTML = "";
  for (const seg of TRIAGE) {
    const li = document.createElement("li");
    li.innerHTML =
      '<span class="legend-swatch" style="background:' + seg.color + '"></span>' +
      '<span class="legend-name"><span class="legend-emoji">' + seg.emoji + "</span>" + seg.label + "</span>" +
      '<span class="legend-count">' + clinical.buckets[seg.key] + "</span>";
    host.appendChild(li);
  }
}

// --- Time series (area + line + hover crosshair) ---
function renderTimeSeries(host, hist) {
  host.innerHTML = "";
  const W = host.clientWidth || 640, H = host.clientHeight || 200;
  const pad = { l: 40, r: 14, t: 14, b: 22 };
  const svg = document.createElementNS(SVGNS, "svg");
  svg.setAttribute("viewBox", "0 0 " + W + " " + H);

  const defs = document.createElementNS(SVGNS, "defs");
  defs.innerHTML = '<linearGradient id="signalfill" x1="0" y1="0" x2="0" y2="1">' +
    '<stop offset="0%" stop-color="var(--signal)" stop-opacity="0.30"/>' +
    '<stop offset="100%" stop-color="var(--signal)" stop-opacity="0.02"/></linearGradient>';
  svg.appendChild(defs);

  const data = hist.length ? hist : [{ t: Date.now(), delivered: 0 }];
  const maxY = Math.max(1, ...data.map((d) => d.delivered));
  const x0 = pad.l, x1 = W - pad.r, y0 = H - pad.b, y1 = pad.t;
  const sx = (i) => x0 + (data.length <= 1 ? 0 : (i / (data.length - 1)) * (x1 - x0));
  const sy = (v) => y0 - (v / maxY) * (y0 - y1);

  // gridlines + y labels
  for (let g = 0; g <= 2; g++) {
    const v = Math.round((maxY / 2) * g);
    const yy = sy(v);
    const line = document.createElementNS(SVGNS, "line");
    line.setAttribute("class", "grid-line");
    line.setAttribute("x1", x0); line.setAttribute("x2", x1); line.setAttribute("y1", yy); line.setAttribute("y2", yy);
    svg.appendChild(line);
    const lbl = document.createElementNS(SVGNS, "text");
    lbl.setAttribute("class", "axis-text"); lbl.setAttribute("x", 6); lbl.setAttribute("y", yy + 3);
    lbl.textContent = v;
    svg.appendChild(lbl);
  }

  let line = "", area = "";
  data.forEach((d, i) => {
    const X = sx(i), Y = sy(d.delivered);
    line += (i === 0 ? "M" : "L") + X.toFixed(1) + " " + Y.toFixed(1) + " ";
  });
  area = line + "L" + sx(data.length - 1).toFixed(1) + " " + y0 + " L" + x0 + " " + y0 + " Z";

  const areaEl = document.createElementNS(SVGNS, "path");
  areaEl.setAttribute("class", "series-area"); areaEl.setAttribute("d", area);
  svg.appendChild(areaEl);
  const lineEl = document.createElementNS(SVGNS, "path");
  lineEl.setAttribute("class", "series-line"); lineEl.setAttribute("d", line);
  svg.appendChild(lineEl);

  // hover layer
  const cross = document.createElementNS(SVGNS, "line");
  cross.setAttribute("class", "crosshair"); cross.setAttribute("y1", y1); cross.setAttribute("y2", y0); cross.style.display = "none";
  svg.appendChild(cross);
  const dot = document.createElementNS(SVGNS, "circle");
  dot.setAttribute("class", "cursor-dot"); dot.setAttribute("r", 4); dot.style.display = "none";
  svg.appendChild(dot);

  const overlay = document.createElementNS(SVGNS, "rect");
  overlay.setAttribute("x", x0); overlay.setAttribute("y", y1); overlay.setAttribute("width", Math.max(0, x1 - x0)); overlay.setAttribute("height", Math.max(0, y0 - y1));
  overlay.setAttribute("fill", "transparent");
  overlay.addEventListener("mousemove", (e) => {
    const rect = svg.getBoundingClientRect();
    const px = (e.clientX - rect.left) * (W / rect.width);
    const i = Math.max(0, Math.min(data.length - 1, Math.round(((px - x0) / Math.max(1, x1 - x0)) * (data.length - 1))));
    const X = sx(i), Y = sy(data[i].delivered);
    cross.style.display = ""; cross.setAttribute("x1", X); cross.setAttribute("x2", X);
    dot.style.display = ""; dot.setAttribute("cx", X); dot.setAttribute("cy", Y);
    showTip(e.clientX, rect.top + (Y / H) * rect.height,
      '<span class="tt-key">delivered</span> ' + data[i].delivered + " · " + clockAgo(data[i].t));
  });
  overlay.addEventListener("mouseleave", () => { cross.style.display = "none"; dot.style.display = "none"; hideTip(); });
  svg.appendChild(overlay);

  host.appendChild(svg);
}

function renderSparkline(host, hist) {
  host.innerHTML = "";
  const W = host.clientWidth || 300, H = host.clientHeight || 46;
  const svg = document.createElementNS(SVGNS, "svg");
  svg.setAttribute("viewBox", "0 0 " + W + " " + H);
  const data = hist.length ? hist.map((d) => d.retries) : [0];
  const maxY = Math.max(1, ...data);
  const sx = (i) => data.length <= 1 ? 0 : (i / (data.length - 1)) * W;
  const sy = (v) => H - 4 - (v / maxY) * (H - 10);
  let line = "";
  data.forEach((v, i) => { line += (i === 0 ? "M" : "L") + sx(i).toFixed(1) + " " + sy(v).toFixed(1) + " "; });
  const area = line + "L" + W + " " + H + " L0 " + H + " Z";
  const a = document.createElementNS(SVGNS, "path"); a.setAttribute("class", "spark-area"); a.setAttribute("d", area); svg.appendChild(a);
  const l = document.createElementNS(SVGNS, "path"); l.setAttribute("class", "spark-line"); l.setAttribute("d", line); svg.appendChild(l);
  attachTip(host, () => "Retries now: " + data[data.length - 1] + " · peak " + maxY);
  host.appendChild(svg);
}

function renderFooter(m) {
  const parts = [];
  parts.push("field " + sourceStatus.field);
  parts.push("gateway " + sourceStatus.gw);
  parts.push("clinical " + (m.clinicalSource === "triage" ? "· field triage" : m.clinicalSource === "flags" ? "· gateway flags" : "· —"));
  if (demoActive) parts.push("· DEMO DATA (no live source)");
  $("foot-source").textContent = parts.join("  ·  ");
}

function renderTable(m) {
  const t = m.transport;
  const rows = [
    ["Delivered", t.delivered], ["Queued", t.queued], ["Sending", t.sending], ["Stuck", t.stuck],
    ["Attempted", t.attempted], ["Success rate", t.successRate == null ? "—" : Math.round(t.successRate * 100) + "%"],
    ["Retries (now)", t.retries], ["In flight (decoding)", t.inflight.length],
    ["Patients", m.clinical.total], ["Critical", m.clinical.buckets.critical], ["High", m.clinical.buckets.high],
    ["Moderate", m.clinical.buckets.moderate], ["Stable", m.clinical.buckets.stable],
  ];
  $("data-table-body").innerHTML = rows.map((r) => "<tr><td>" + r[0] + "</td><td>" + r[1] + "</td></tr>").join("");
}

// ---------------------------------------------------------------------------
// Signature: animated link wave
// ---------------------------------------------------------------------------
let waveAmp = 0.5, wavePhase = 0, waveRAF = null;
const reduceMotion = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

function updateWave(rate) {
  waveAmp = rate; // higher success → calmer, fuller wave; lower → jagged
  if (reduceMotion) { drawWave(0); return; }
  if (waveRAF == null) tickWave();
}
function tickWave() {
  wavePhase += 0.05;
  drawWave(wavePhase);
  waveRAF = requestAnimationFrame(tickWave);
}
function drawWave(phase) {
  const W = 600, H = 80, mid = H / 2;
  const amp = 6 + waveAmp * 20;        // amplitude grows with success rate
  const noise = (1 - waveAmp) * 10;    // jitter grows as delivery falters
  const build = (shift) => {
    let d = "M0 " + mid;
    for (let x = 0; x <= W; x += 12) {
      const y = mid + Math.sin(x / 42 + phase + shift) * amp + (Math.sin(x / 9 + phase * 2) * noise * 0.5);
      d += " L" + x + " " + y.toFixed(1);
    }
    return d;
  };
  const front = document.getElementById("wave-front");
  const back = document.getElementById("wave-back");
  if (front) front.setAttribute("d", build(0));
  if (back) back.setAttribute("d", build(1.6));
}

// ---------------------------------------------------------------------------
// Tooltip helpers
// ---------------------------------------------------------------------------
let tipEl = null;
function ensureTip() {
  if (!tipEl) { tipEl = document.createElement("div"); tipEl.className = "tt"; tipEl.hidden = true; document.body.appendChild(tipEl); }
  return tipEl;
}
function showTip(x, y, html) {
  const el = ensureTip();
  el.innerHTML = html; el.hidden = false;
  el.style.left = x + "px"; el.style.top = y + "px";
}
function hideTip() { if (tipEl) tipEl.hidden = true; }
function attachTip(el, text) {
  el.addEventListener("mousemove", (e) => showTip(e.clientX, e.clientY, escapeHtml(text())));
  el.addEventListener("mouseleave", hideTip);
}
function escapeHtml(s) {
  return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

// ---------------------------------------------------------------------------
// Time helpers
// ---------------------------------------------------------------------------
function clockAgo(t) {
  const s = Math.round((Date.now() - t) / 1000);
  if (s < 60) return s + "s ago";
  return Math.round(s / 60) + "m ago";
}
function tickUptime() {
  const s = Math.floor((Date.now() - startedAt) / 1000);
  const hh = String(Math.floor(s / 3600)).padStart(2, "0");
  const mm = String(Math.floor((s % 3600) / 60)).padStart(2, "0");
  const ss = String(s % 60).padStart(2, "0");
  $("uptime").textContent = hh + ":" + mm + ":" + ss;
}

// ---------------------------------------------------------------------------
// Settings drawer + table toggle
// ---------------------------------------------------------------------------
function openDrawer() {
  $("input-field-url").value = cfg.fieldUrl;
  $("input-gw-url").value = cfg.gwUrl;
  $("drawer").hidden = false; $("drawer-scrim").hidden = false;
  $("input-field-url").focus();
}
function closeDrawer() { $("drawer").hidden = true; $("drawer-scrim").hidden = true; }
function applyDrawer() {
  const f = $("input-field-url").value.trim() || DEFAULTS.fieldUrl;
  const g = $("input-gw-url").value.trim() || DEFAULTS.gwUrl;
  saveConfig({ fieldUrl: f, gwUrl: g });
  closeDrawer();
  poll().catch(() => {});
}

function wireUI() {
  $("btn-settings").addEventListener("click", openDrawer);
  $("btn-drawer-cancel").addEventListener("click", closeDrawer);
  $("drawer-scrim").addEventListener("click", closeDrawer);
  $("btn-drawer-save").addEventListener("click", applyDrawer);
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeDrawer(); });

  const tbl = $("data-table"), btn = $("btn-table");
  btn.addEventListener("click", () => {
    const show = tbl.hidden;
    tbl.hidden = !show;
    btn.setAttribute("aria-expanded", String(show));
  });

  let resizeRAF = null;
  window.addEventListener("resize", () => {
    if (resizeRAF) cancelAnimationFrame(resizeRAF);
    resizeRAF = requestAnimationFrame(() => { if (lastMetrics) { renderTimeSeries($("chart-delivery"), history); renderSparkline($("spark-retries"), history); } });
  });
}

// ---------------------------------------------------------------------------
// Self-test (open with ?selftest to assert the pure functions in the console)
// ---------------------------------------------------------------------------
function runSelfTest() {
  const results = [];
  const check = (name, cond) => results.push({ name, pass: !!cond });

  const t1 = computeMetrics({
    fieldStatus: { queue: [
      { state: "delivered", retries: 0 }, { state: "delivered", retries: 2 },
      { state: "sending", retries: 1 }, { state: "queued", retries: 0 }, { state: "stuck", retries: 8 },
    ] },
    gwQueue: [], gwObs: null, cases: [],
  });
  check("counts by state", t1.transport.delivered === 2 && t1.transport.sending === 1 && t1.transport.queued === 1 && t1.transport.stuck === 1);
  check("retries summed", t1.transport.retries === 11);
  check("success rate delivered/(delivered+stuck)", Math.abs(t1.transport.successRate - 2 / 3) < 1e-9);

  const t2 = computeMetrics({ fieldStatus: null, gwQueue: [
    { bundle_id: "aabbccdd11", state: "receiving", symbols_received: 6, symbols_needed: 12 },
    { bundle_id: "eeff", state: "complete" },
  ], gwObs: [{ bundle_id: "x", flags: [] }], cases: [] });
  check("gateway-only inflight pct", t2.transport.inflight[0].pct === 50);
  check("delivered prefers observations", t2.transport.delivered === 1);

  const t3 = computeClinical([{ assessment: { priority: "critical" } }, { assessment: { priority: "stable" } }], null);
  check("clinical from triage", t3.buckets.critical === 1 && t3.buckets.stable === 1 && t3.source === "triage");

  const t4 = computeClinical([], [{ flags: [] }, { flags: ["a", "b"] }, { flags: ["a", "b", "c"] }]);
  check("clinical flags proxy buckets", t4.buckets.stable === 1 && t4.buckets.high === 1 && t4.buckets.critical === 1 && t4.source === "flags");

  const passed = results.filter((r) => r.pass).length;
  console.table(results);
  console.log("[selftest] " + passed + "/" + results.length + " passed");
  return passed === results.length;
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------
function boot() {
  wireUI();
  drawWave(0);
  tickUptime();
  setInterval(tickUptime, 1000);
  poll().catch(() => {});
  setInterval(() => poll().catch(() => {}), POLL_MS);
  if (new URLSearchParams(location.search).has("selftest")) runSelfTest();
}

if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot);
else boot();
