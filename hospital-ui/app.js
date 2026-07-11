/* =====================================================
   Hospital Emergency Dashboard - app.js

   Reads live clinical data from the gateway HTTP API
   (Contract 3): GET /api/observations, GET /api/queue,
   GET /api/images/{bundle_id}. These are fed by the real
   UDP + RaptorQ FEC delivery path (field client -> gateway
   -> redb store), so what shows here is what actually
   survived the lossy link.

   Served from the gateway's own static dir, so API_BASE is
   empty (same origin) and no CORS is needed. Set API_BASE to
   "http://host:8080" only when serving the dashboard from a
   different origin (requires a CORS layer on the gateway).
   ===================================================== */

// --- Configuration ---
const API_BASE = "";
const POLL_INTERVAL = 2000; // ms

function apiUrl(endpoint) {
  const map = { observations: "/api/observations", queue: "/api/queue" };
  return API_BASE + (map[endpoint] || endpoint);
}
function imageUrl(path) {
  if (!path) return null;
  return API_BASE + path;
}

// --- Clinical constants ---
const PRIORITY_ORDER = ["critical", "high", "moderate", "stable"];
const PRIORITY_META = {
  critical: { emoji: "\u{1F534}", label: "Critical", color: "Red" },
  high:     { emoji: "\u{1F7E0}", label: "High",     color: "Orange" },
  moderate: { emoji: "\u{1F7E1}", label: "Moderate", color: "Yellow" },
  stable:   { emoji: "\u{1F7E2}", label: "Stable",   color: "Green" },
};

const WORKFLOW_STEPS = [
  { key: "waiting",         label: "Waiting" },
  { key: "viewed",          label: "Viewed" },
  { key: "under_treatment", label: "Under Treatment" },
  { key: "transferred",     label: "Transferred" },
  { key: "completed",       label: "Completed" },
];
const WORKFLOW_LABEL = WORKFLOW_STEPS.reduce((m, s) => (m[s.key] = s.label, m), {});
const TIMELINE_LABEL = {
  viewed: "Viewed by ER Doctor",
  under_treatment: "Treatment Started",
  transferred: "Transferred to ICU",
  completed: "Completed",
};

// LOINC codes emitted by tgw-fhir.
const LOINC = { BP_PANEL: "85354-9", SYSTOLIC: "8480-6", DIASTOLIC: "8462-4", SPO2: "59408-5", PULSE: "8867-4" };

const XFER_STATE = {
  receiving:    { label: "Receiving",     icon: "◌", cls: "wf-waiting" },
  complete:     { label: "Complete",      icon: "●", cls: "wf-under_treatment" },
  receipt_sent: { label: "Receipt Sent",  icon: "✓", cls: "wf-completed" },
};

// --- State ---
let cases = [];
let selectedId = null;
const seenIds = new Set();
let firstLoad = true;
let pollInFlight = false;
let viewerRotation = 0;
let viewerZoom = 1;

// Workflow is a dashboard-side concept (the gateway carries no triage workflow),
// so it lives in the browser, keyed by patient id, and is merged in on each poll.
const workflowState = new Map(); // patientId -> { status, timeline:[], completedAt }

// --- DOM refs ---
const $queuePanel   = document.getElementById("queue-panel");
const $detailPanel  = document.getElementById("detail-panel");
const $detailEmpty  = document.getElementById("detail-empty");
const $detailContent= document.getElementById("detail-content");
const $btnBack      = document.getElementById("btn-back");
const $hospShell    = document.querySelector(".hosp-shell");
const $connBadge    = document.getElementById("conn-badge");

const $viewer     = document.getElementById("image-viewer");
const $viewerImg  = document.getElementById("viewer-img");
const $viewerStage= document.getElementById("viewer-stage");

// =====================================================
// Helpers
// =====================================================

function escHtml(str) {
  if (str === null || str === undefined) return "";
  return String(str).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}
function escAttr(str) { return escHtml(str); }

function formatRelative(iso) {
  if (!iso) return "--";
  const diffMs = Date.now() - new Date(iso).getTime();
  const mins = Math.floor(diffMs / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return mins + " min ago";
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return hrs + "h " + (mins % 60) + "m ago";
  return Math.floor(hrs / 24) + "d ago";
}

function formatClock(iso) {
  if (!iso) return "--:--";
  return new Date(iso).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
}

function priorityOf(c) {
  const p = c.assessment && c.assessment.priority;
  return PRIORITY_ORDER.includes(p) ? p : "stable";
}

function infoItem(label, value, opts) {
  opts = opts || {};
  const isEmpty = value === null || value === undefined || value === "" || (value === false && !opts.isBool);
  const display = opts.isBool ? (value ? "Yes" : "No") : (isEmpty ? "Not recorded" : escHtml(value));
  return (
    '<div class="info-item">' +
      '<span class="info-label">' + escHtml(label) + '</span>' +
      '<span class="info-value' + (isEmpty && !opts.isBool ? " muted" : "") + '">' + display + '</span>' +
    '</div>'
  );
}

// =====================================================
// Gateway API -> case model
// =====================================================

function fhirCode(fhir) {
  return (fhir && fhir.code && fhir.code.coding && fhir.code.coding[0] && fhir.code.coding[0].code) || null;
}
function fhirValue(fhir) {
  return (fhir && fhir.valueQuantity && typeof fhir.valueQuantity.value === "number") ? fhir.valueQuantity.value : null;
}
function fhirComponent(fhir, code) {
  if (!fhir || !Array.isArray(fhir.component)) return null;
  const comp = fhir.component.find(c => c.code && c.code.coding && c.code.coding[0] && c.code.coding[0].code === code);
  return comp && comp.valueQuantity && typeof comp.valueQuantity.value === "number" ? comp.valueQuantity.value : null;
}

// Fold one FHIR observation's decoded values into a patient's vitals.
function foldVital(vitals, fhir) {
  const code = fhirCode(fhir);
  if (code === LOINC.BP_PANEL) {
    const sys = fhirComponent(fhir, LOINC.SYSTOLIC);
    const dia = fhirComponent(fhir, LOINC.DIASTOLIC);
    if (sys !== null) vitals.bpSys = sys;
    if (dia !== null) vitals.bpDia = dia;
  } else if (code === LOINC.SPO2) {
    const v = fhirValue(fhir);
    if (v !== null) vitals.spo2 = v;
  } else if (code === LOINC.PULSE) {
    const v = fhirValue(fhir);
    if (v !== null) vitals.pulse = v;
  }
}

// Build the case array the renderer consumes from raw API payloads.
function transformApi(observations, queue) {
  // Map bundle_id -> patient_id, so queue (FEC) entries can attach to a patient.
  const bundleToPatient = new Map();
  const byPatient = new Map();

  const ensure = (pid) => {
    if (!byPatient.has(pid)) {
      byPatient.set(pid, {
        bundleId: pid,
        patientId: pid,
        receivedAt: null,
        vitals: { spo2: null, pulse: null, bpSys: null, bpDia: null },
        flags: [],
        images: [],
        fhir: [],
        transfers: [],
        bundleIds: new Set(),
      });
    }
    return byPatient.get(pid);
  };

  for (const obs of observations || []) {
    const pid = obs.patient_id || "Unknown";
    const rec = ensure(pid);
    rec.bundleIds.add(obs.bundle_id);
    bundleToPatient.set(obs.bundle_id, pid);
    if (!rec.receivedAt || new Date(obs.received_at) > new Date(rec.receivedAt)) rec.receivedAt = obs.received_at;

    if (obs.kind === "vitals") {
      foldVital(rec.vitals, obs.fhir);
      if (Array.isArray(obs.flags)) rec.flags.push(...obs.flags);
      if (obs.fhir) rec.fhir.push(obs.fhir);
    } else if (obs.kind === "image") {
      const url = imageUrl(obs.image_url);
      if (url) rec.images.push({ dataUrl: url, bundleId: obs.bundle_id });
    }
  }

  // Attach FEC / delivery transfer state from the queue.
  for (const q of queue || []) {
    const pid = bundleToPatient.get(q.bundle_id);
    if (!pid) continue; // in-flight bundle not yet delivered/decoded -> no patient yet
    ensure(pid).transfers.push({
      bundleId: q.bundle_id,
      state: q.state,
      symbolsReceived: q.symbols_received,
      symbolsNeeded: q.symbols_needed,
      firstSeen: q.first_seen,
      completedAt: q.completed_at,
    });
  }

  const out = [];
  for (const rec of byPatient.values()) {
    const c = {
      bundleId: rec.bundleId,
      patientId: rec.patientId,
      receivedAt: rec.receivedAt,
      breathing: { spo2: rec.vitals.spo2 },
      circulation: {
        pulse: rec.vitals.pulse,
        bpSys: rec.vitals.bpSys,
        bpDia: rec.vitals.bpDia,
        shockIndex: (rec.vitals.pulse !== null && rec.vitals.bpSys) ? rec.vitals.pulse / rec.vitals.bpSys : null,
      },
      flags: dedupe(rec.flags),
      images: rec.images,
      fhir: rec.fhir,
      transfers: rec.transfers.sort((a, b) => new Date(a.firstSeen) - new Date(b.firstSeen)),
    };
    c.assessment = derivePriority(c);

    // Merge browser-side workflow state.
    const wf = workflowState.get(c.patientId);
    c.workflowStatus = wf ? wf.status : "waiting";
    c.completedAt = wf ? wf.completedAt : null;
    c.timeline = buildTimeline(c, wf);
    out.push(c);
  }
  return out;
}

function dedupe(arr) { return Array.from(new Set(arr)); }

function buildTimeline(c, wf) {
  const events = [];
  if (c.receivedAt) events.push({ ts: c.receivedAt, label: "Assessment Received (FEC delivered)" });
  if (wf && wf.timeline) events.push(...wf.timeline);
  return events.sort((a, b) => new Date(a.ts) - new Date(b.ts));
}

// Derive urgency from the vitals that actually arrived + plausibility flags.
// The gateway transmits no triage score, so this is the dashboard's own read.
function derivePriority(c) {
  const spo2 = c.breathing.spo2;
  const sys = c.circulation.bpSys;
  const pulse = c.circulation.pulse;
  const findings = [];
  if (sys !== null && c.circulation.bpDia !== null) findings.push("BP " + sys + "/" + c.circulation.bpDia + " mmHg");
  else if (sys !== null) findings.push("Systolic " + sys + " mmHg");
  if (spo2 !== null) findings.push("SpO2 " + spo2 + "%");
  if (pulse !== null) findings.push("Pulse " + pulse + " bpm");
  if (c.circulation.shockIndex !== null) findings.push("Shock Index " + c.circulation.shockIndex.toFixed(2));
  c.flags.forEach(f => findings.push("Plausibility flag: " + f));

  let priority = "stable";
  const bump = (p) => { if (PRIORITY_ORDER.indexOf(p) < PRIORITY_ORDER.indexOf(priority)) priority = p; };

  if (spo2 !== null && spo2 <= 91) bump("critical");
  else if (spo2 !== null && spo2 <= 93) bump("high");
  else if (spo2 !== null && spo2 <= 95) bump("moderate");

  if (sys !== null && sys <= 90) bump("critical");
  else if (sys !== null && sys <= 100) bump("high");
  else if (sys !== null && sys <= 110) bump("moderate");

  if (pulse !== null && (pulse >= 131 || pulse <= 40)) bump("critical");
  else if (pulse !== null && (pulse >= 111 || pulse <= 50)) bump("high");
  else if (pulse !== null && pulse >= 91) bump("moderate");

  if (c.flags.length) bump("moderate");

  const meta = PRIORITY_META[priority];
  return { priority, priorityLabel: meta.label, findings, news2: null, qsofa: null };
}

function computeAlerts(c) {
  const spo2 = c.breathing.spo2, sys = c.circulation.bpSys, pulse = c.circulation.pulse;
  const alerts = [];
  if (spo2 !== null && spo2 <= 91) alerts.push({ title: "Respiratory Failure", reason: "SpO2 " + spo2 + "% (≤91%) indicates critical hypoxaemia." });
  if (sys !== null && sys <= 90) alerts.push({ title: "Severe Hypotension", reason: "Systolic BP " + sys + " mmHg (≤90)." + (c.circulation.shockIndex ? " Shock index " + c.circulation.shockIndex.toFixed(2) + "." : "") });
  if (pulse !== null && pulse >= 131) alerts.push({ title: "Severe Tachycardia", reason: "Pulse " + pulse + " bpm (≥131)." });
  if (pulse !== null && pulse <= 40) alerts.push({ title: "Severe Bradycardia", reason: "Pulse " + pulse + " bpm (≤40)." });
  c.flags.forEach(f => alerts.push({ title: "Implausible Reading", reason: "Gateway flagged: " + f + "." }));
  return alerts;
}

// =====================================================
// Rendering — queue (master)
// =====================================================

function vitalsSummary(c) {
  const parts = [];
  if (c.circulation.bpSys !== null && c.circulation.bpDia !== null) parts.push("BP " + c.circulation.bpSys + "/" + c.circulation.bpDia);
  if (c.breathing.spo2 !== null) parts.push("SpO2 " + c.breathing.spo2 + "%");
  if (c.circulation.pulse !== null) parts.push("HR " + c.circulation.pulse);
  return parts.length ? parts.join(" · ") : "Awaiting vitals";
}

function deliveryLabel(c) {
  if (!c.transfers.length) return "--";
  const anyReceipt = c.transfers.some(t => t.state === "receipt_sent");
  const allDone = c.transfers.every(t => t.state === "receipt_sent" || t.state === "complete");
  if (anyReceipt || allDone) return "Delivered ✓";
  return "Receiving…";
}

function buildQueueCardHtml(c) {
  const p = priorityOf(c);
  const meta = PRIORITY_META[p];
  const status = c.workflowStatus || "waiting";
  const isNew = !seenIds.has(c.bundleId);
  const isSelected = c.bundleId === selectedId;

  return (
    '<div class="queue-card risk-' + p + (isSelected ? " selected" : "") + (isNew ? " new-arrival" : "") + '" ' +
         'data-bundle-id="' + escAttr(c.bundleId) + '" role="button" tabindex="0">' +
      '<div class="qc-top"><span class="qc-priority-chip">' + meta.emoji + " " + meta.label.toUpperCase() + '</span></div>' +
      '<div class="qc-patient-id">' + escHtml(c.patientId || "--") + '</div>' +
      '<div class="qc-meta">' + escHtml(deliveryLabel(c)) + '</div>' +
      '<div class="qc-complaint-label">Vitals</div>' +
      '<div class="qc-complaint-value">' + escHtml(vitalsSummary(c)) + '</div>' +
      '<div class="qc-scores"><span>' + (c.flags.length ? "⚠ " + c.flags.length + " flag(s)" : "No plausibility flags") + '</span></div>' +
      '<div class="qc-bottom">' +
        '<span>Received: ' + formatRelative(c.receivedAt) + '</span>' +
        '<span class="qc-status wf-' + status + '">' + (WORKFLOW_LABEL[status] || status) + '</span>' +
      '</div>' +
    '</div>'
  );
}

function render(allCases) {
  cases = allCases || [];

  const active = cases.filter(c => c.workflowStatus !== "completed");
  const completed = cases.filter(c => c.workflowStatus === "completed")
    .sort((a, b) => new Date(b.completedAt || b.receivedAt) - new Date(a.completedAt || a.receivedAt));

  const grouped = { critical: [], high: [], moderate: [], stable: [] };
  active.forEach(c => grouped[priorityOf(c)].push(c));
  PRIORITY_ORDER.forEach(p => grouped[p].sort((a, b) => new Date(b.receivedAt) - new Date(a.receivedAt)));

  PRIORITY_ORDER.forEach(p => {
    document.getElementById("cards-" + p).innerHTML = grouped[p].map(buildQueueCardHtml).join("");
    document.getElementById("pcount-" + p).textContent = grouped[p].length;
  });
  document.getElementById("queue-empty").hidden = active.length > 0;
  document.getElementById("count-active").textContent = active.length;

  document.getElementById("cards-completed").innerHTML = completed.map(buildQueueCardHtml).join("");
  document.getElementById("completed-empty").hidden = completed.length > 0;
  document.getElementById("count-completed").textContent = completed.length;

  cases.forEach(c => seenIds.add(c.bundleId));

  if (selectedId) {
    const sel = cases.find(c => c.bundleId === selectedId);
    if (sel) renderDetail(sel);
  }
}

// =====================================================
// Rendering — detail (right panel)
// =====================================================

function renderTransferCard(t) {
  const meta = XFER_STATE[t.state] || { label: t.state, icon: "", cls: "" };
  const pct = t.symbolsNeeded > 0 ? Math.min(100, Math.round((t.symbolsReceived / t.symbolsNeeded) * 100)) : 100;
  return (
    '<div class="info-item" style="align-items:flex-start;">' +
      '<span class="info-label" title="' + escAttr(t.bundleId) + '">' + escHtml(t.bundleId.substring(0, 8)) + '…</span>' +
      '<span class="info-value">' +
        '<span class="qc-status ' + meta.cls + '">' + meta.icon + ' ' + escHtml(meta.label) + '</span> ' +
        escHtml(t.symbolsReceived + "/" + t.symbolsNeeded + " symbols (" + pct + "%)") +
        (t.completedAt ? " · " + formatClock(t.completedAt) : "") +
      '</span>' +
    '</div>'
  );
}

function renderDetail(c) {
  const p = priorityOf(c);
  const meta = PRIORITY_META[p];
  const alerts = computeAlerts(c);
  const reasons = (c.assessment && c.assessment.findings) || [];

  const photoThumb = (c.images && c.images.length)
    ? '<img class="patient-photo-thumb" src="' + escAttr(c.images[0].dataUrl) + '" alt="Patient photo" data-viewer-index="0">'
    : '<div class="patient-photo-thumb" style="display:flex;align-items:center;justify-content:center;font-size:0.65rem;color:var(--text-secondary);text-align:center;">No Photo</div>';

  const html = `
    <section class="med-card">
      <h2 class="card-title">Patient Information</h2>
      <div class="card-body">
        <div class="patient-id-row">
          ${photoThumb}
          <div class="info-grid" style="flex:1;">
            ${infoItem("Patient ID", c.patientId)}
            ${infoItem("Received", formatClock(c.receivedAt) + " (" + formatRelative(c.receivedAt) + ")")}
            ${infoItem("Delivery", deliveryLabel(c))}
            ${infoItem("Bundles", c.transfers.length || c.fhir.length || "--")}
          </div>
        </div>
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Vital Signs</h2>
      <div class="card-body">
        <div class="info-grid">
          ${infoItem("Blood Pressure", (c.circulation.bpSys !== null && c.circulation.bpDia !== null) ? c.circulation.bpSys + "/" + c.circulation.bpDia + " mmHg" : null)}
          ${infoItem("SpO₂", c.breathing.spo2 !== null ? c.breathing.spo2 + "%" : null)}
          ${infoItem("Pulse", c.circulation.pulse !== null ? c.circulation.pulse + " bpm" : null)}
          ${infoItem("Shock Index", c.circulation.shockIndex !== null ? c.circulation.shockIndex.toFixed(2) : null)}
        </div>
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Delivery — RaptorQ FEC</h2>
      <div class="card-body">
        ${c.transfers.length
          ? '<div class="info-grid">' + c.transfers.map(renderTransferCard).join("") + '</div>'
          : '<p class="info-value muted">No transfer records for this patient.</p>'}
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Clinical Image</h2>
      <div class="card-body">
        ${(c.images && c.images.length)
          ? '<div class="image-gallery">' + c.images.map((img, i) =>
              '<div class="gallery-thumb" data-viewer-index="' + i + '"><img src="' + escAttr(img.dataUrl) + '" alt="Field image ' + (i + 1) + ' for ' + escAttr(c.patientId) + '" loading="lazy"></div>'
            ).join("") + '</div>'
          : '<p class="info-value muted">No image delivered</p>'}
      </div>
    </section>

    <section class="med-card assessment-card risk-${p}">
      <h2 class="card-title">Clinical Summary</h2>
      <div class="card-body">
        <div class="summary-priority-row">
          <span class="risk-dot risk-${p}"></span>
          <span class="summary-priority-badge">${meta.emoji} ${escHtml(meta.label.toUpperCase())}</span>
        </div>
        <div class="info-grid">
          ${infoItem("Priority", meta.label)}
          ${infoItem("Triage Category", meta.emoji + " " + meta.color)}
          ${infoItem("Shock Index", c.circulation.shockIndex !== null ? c.circulation.shockIndex.toFixed(2) : null)}
          ${infoItem("Plausibility Flags", c.flags.length ? c.flags.join(", ") : null)}
        </div>
        <p class="info-value muted" style="margin-top:10px;font-size:0.72rem;">Priority is derived on the dashboard from delivered vitals; the gateway transmits no triage score.</p>
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Why This Patient Was Ranked</h2>
      <div class="card-body">
        <p class="info-value">Priority: <strong>${escHtml((c.assessment && c.assessment.priorityLabel) || "--")}</strong></p>
        <h3 class="explain-subheading" style="margin-top:12px;">Delivered Findings</h3>
        ${reasons.length ? '<ul class="reason-list">' + reasons.map(r => '<li>' + escHtml(r) + '</li>').join("") + '</ul>' : '<p class="info-value muted">No vitals delivered yet.</p>'}
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Active Alerts</h2>
      <div class="card-body">
        ${alerts.length
          ? alerts.map(a => '<div class="alert-item"><span class="alert-icon">\u{1F6A8}</span><div><div class="alert-title">' + escHtml(a.title) + '</div><div class="alert-reason">' + escHtml(a.reason) + '</div></div></div>').join("")
          : '<p class="no-alerts">No active alerts.</p>'}
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">FHIR R5 (as delivered)</h2>
      <div class="card-body">
        ${c.fhir.length
          ? '<details class="fhir-details"><summary class="explain-subheading" style="cursor:pointer;">View ' + c.fhir.length + ' FHIR Observation(s)</summary><pre class="fhir-pre">' + escHtml(JSON.stringify(c.fhir, null, 2)) + '</pre></details>'
          : '<p class="info-value muted">No FHIR observations.</p>'}
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Workflow Controls</h2>
      <div class="card-body">
        <div class="workflow-row">
          ${WORKFLOW_STEPS.map(s => '<button type="button" class="workflow-btn wf-' + s.key + (c.workflowStatus === s.key ? " active" : "") + '" data-workflow="' + s.key + '">' + escHtml(s.label) + '</button>').join('<span class="workflow-arrow">→</span>')}
        </div>
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Timeline</h2>
      <div class="card-body">
        <ul class="timeline-list">
          ${(c.timeline || []).map(t => '<li class="timeline-item"><div class="timeline-time">' + formatClock(t.ts) + '</div><div class="timeline-label">' + escHtml(t.label) + '</div></li>').join("")}
        </ul>
      </div>
    </section>
  `;

  $detailContent.innerHTML = html;
  $detailContent.dataset.bundleId = c.bundleId;
  $detailEmpty.hidden = true;
  $detailContent.hidden = false;
}

// =====================================================
// Selection + workflow
// =====================================================

function isMobile() { return window.matchMedia("(max-width: 900px)").matches; }

function selectCase(id) {
  selectedId = id;
  const sel = cases.find(c => c.bundleId === id);
  if (!sel) return;
  renderDetail(sel);
  document.querySelectorAll(".queue-card").forEach(el => {
    el.classList.toggle("selected", el.dataset.bundleId === id);
  });
  if (isMobile()) $hospShell.classList.add("detail-open");
}

function closeDetailDrawer() {
  $hospShell.classList.remove("detail-open");
}

function updateWorkflow(patientId, newStatus) {
  const prev = workflowState.get(patientId) || { status: "waiting", timeline: [], completedAt: null };
  if (prev.status === newStatus) return;
  const next = { status: newStatus, timeline: [...prev.timeline], completedAt: prev.completedAt };
  const label = TIMELINE_LABEL[newStatus];
  if (label) next.timeline.push({ ts: new Date().toISOString(), label });
  if (newStatus === "completed") next.completedAt = new Date().toISOString();
  workflowState.set(patientId, next);

  // Re-apply to the in-memory case and re-render immediately.
  const c = cases.find(x => x.patientId === patientId);
  if (c) {
    c.workflowStatus = next.status;
    c.completedAt = next.completedAt;
    c.timeline = buildTimeline(c, next);
  }
  render(cases);
}

// =====================================================
// Image viewer
// =====================================================

function openViewer(dataUrl) {
  viewerRotation = 0;
  viewerZoom = 1;
  $viewerImg.src = dataUrl;
  applyViewerTransform();
  $viewer.hidden = false;
}
function applyViewerTransform() {
  $viewerImg.style.transform = "scale(" + viewerZoom + ") rotate(" + viewerRotation + "deg)";
}
function closeViewer() {
  $viewer.hidden = true;
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
}

// =====================================================
// Connection status
// =====================================================

function setConnected(ok) {
  if (!$connBadge) return;
  $connBadge.classList.toggle("conn-live", ok);
  $connBadge.classList.toggle("conn-offline", !ok);
  const dot = $connBadge.querySelector(".conn-dot");
  $connBadge.textContent = "";
  if (dot) $connBadge.appendChild(dot);
  else { const d = document.createElement("span"); d.className = "conn-dot"; $connBadge.appendChild(d); }
  $connBadge.append(ok ? " Live" : " Reconnecting…");
}

// =====================================================
// Polling
// =====================================================

async function poll() {
  pollInFlight = true;
  try {
    const [obsRes, queueRes] = await Promise.all([
      fetch(apiUrl("observations")),
      fetch(apiUrl("queue")),
    ]);
    if (!obsRes.ok || !queueRes.ok) throw new Error("HTTP " + obsRes.status + "/" + queueRes.status);
    const observations = await obsRes.json();
    const queue = await queueRes.json();
    render(transformApi(observations, queue));
    firstLoad = false;
    setConnected(true);
  } catch (err) {
    console.warn("[hospital-ui] poll failed:", err.message);
    setConnected(false);
    // Keep last good data on screen.
  } finally {
    pollInFlight = false;
  }
}

function pollGuard() { if (!pollInFlight) poll(); }

// =====================================================
// Event wiring
// =====================================================

function initTabs() {
  document.getElementById("tab-active").addEventListener("click", () => switchTab("active"));
  document.getElementById("tab-completed").addEventListener("click", () => switchTab("completed"));
}

function switchTab(tab) {
  document.getElementById("tab-active").classList.toggle("active", tab === "active");
  document.getElementById("tab-active").setAttribute("aria-selected", tab === "active");
  document.getElementById("tab-completed").classList.toggle("active", tab === "completed");
  document.getElementById("tab-completed").setAttribute("aria-selected", tab === "completed");
  document.getElementById("queue-view-active").hidden = tab !== "active";
  document.getElementById("queue-view-completed").hidden = tab !== "completed";
}

function initQueueDelegation() {
  $queuePanel.addEventListener("click", (e) => {
    const header = e.target.closest(".priority-header");
    if (header) {
      const section = header.closest(".priority-section");
      const collapsed = section.getAttribute("data-collapsed") === "true";
      section.setAttribute("data-collapsed", collapsed ? "false" : "true");
      header.setAttribute("aria-expanded", String(collapsed));
      return;
    }
    const card = e.target.closest(".queue-card");
    if (card) selectCase(card.dataset.bundleId);
  });

  $queuePanel.addEventListener("keydown", (e) => {
    if (e.key !== "Enter" && e.key !== " ") return;
    const card = e.target.closest(".queue-card");
    if (card) { e.preventDefault(); selectCase(card.dataset.bundleId); }
  });
}

function initDetailDelegation() {
  $detailContent.addEventListener("click", (e) => {
    const wfBtn = e.target.closest(".workflow-btn");
    if (wfBtn) {
      updateWorkflow($detailContent.dataset.bundleId, wfBtn.dataset.workflow);
      return;
    }
    const thumb = e.target.closest("[data-viewer-index]");
    if (thumb) {
      const sel = cases.find(c => c.bundleId === $detailContent.dataset.bundleId);
      const idx = parseInt(thumb.dataset.viewerIndex, 10);
      if (sel && sel.images && sel.images[idx]) openViewer(sel.images[idx].dataUrl);
    }
  });
}

function initViewerControls() {
  document.getElementById("viewer-zoom-in").addEventListener("click", () => { viewerZoom = Math.min(4, viewerZoom + 0.25); applyViewerTransform(); });
  document.getElementById("viewer-zoom-out").addEventListener("click", () => { viewerZoom = Math.max(0.5, viewerZoom - 0.25); applyViewerTransform(); });
  document.getElementById("viewer-rotate").addEventListener("click", () => { viewerRotation = (viewerRotation + 90) % 360; applyViewerTransform(); });
  document.getElementById("viewer-fullscreen").addEventListener("click", () => {
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    else $viewerStage.requestFullscreen().catch(() => {});
  });
  document.getElementById("viewer-close").addEventListener("click", closeViewer);
  document.addEventListener("keydown", (e) => { if (e.key === "Escape" && !$viewer.hidden) closeViewer(); });
}

function init() {
  initTabs();
  initQueueDelegation();
  initDetailDelegation();
  initViewerControls();
  $btnBack.addEventListener("click", closeDetailDrawer);

  poll();
  setInterval(pollGuard, POLL_INTERVAL);
  // Keep "Received: X min ago" fresh without disrupting selection.
  setInterval(() => render(cases), 15000);
}

init();
