/* =====================================================
   Hospital Emergency Dashboard - app.js
   Reads patient cases from the shared TgwStore (localStorage
   + BroadcastChannel bridge from field-ui). No polling, no
   backend: same-origin, same-machine demo channel.
   ===================================================== */

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

const AVPU_LABEL = { A: "Alert", C: "New Confusion", V: "Responds to Voice", P: "Responds to Pain", U: "Unresponsive" };

// --- State ---
let cases = [];
let selectedId = null;
let activeTab = "active";
const seenIds = new Set();
let viewerRotation = 0;
let viewerZoom = 1;

// --- DOM refs ---
const $queuePanel   = document.getElementById("queue-panel");
const $detailPanel  = document.getElementById("detail-panel");
const $detailEmpty  = document.getElementById("detail-empty");
const $detailContent= document.getElementById("detail-content");
const $btnBack      = document.getElementById("btn-back");
const $hospShell    = document.querySelector(".hosp-shell");

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

function sexLabel(sex) {
  return sex === "M" ? "Male" : sex === "F" ? "Female" : (sex || "Unknown");
}

function ageLabel(c) {
  if (c.ageYears !== null && c.ageYears !== undefined && c.ageYears !== "") return c.ageYears + " Years";
  return c.ageCategory || "--";
}

function infoItem(label, value, opts) {
  opts = opts || {};
  const isEmpty = value === null || value === undefined || value === "" || value === false && !opts.isBool;
  const display = opts.isBool ? (value ? "Yes" : "No") : (isEmpty ? "Not recorded" : escHtml(value));
  return (
    '<div class="info-item">' +
      '<span class="info-label">' + escHtml(label) + '</span>' +
      '<span class="info-value' + (isEmpty && !opts.isBool ? " muted" : "") + '">' + display + '</span>' +
    '</div>'
  );
}

// =====================================================
// Clinical derivations (presentation-layer only — the
// NEWS2/qSOFA/priority numbers themselves come straight
// from field-ui's own engine, untouched).
// =====================================================

function computeAlerts(c) {
  const b = c.breathing || {}, ci = c.circulation || {}, d = c.disability || {}, a = c.airway || {};
  const flags = c.clinicalFlags || {};
  const news2 = c.assessment ? c.assessment.news2 : null;
  const qsofa = c.assessment ? c.assessment.qsofa : null;
  const alerts = [];

  if (a.status === "Obstructed" || a.status === "Compromised - Intervention Needed") {
    alerts.push({ title: "Airway Compromise", reason: "Airway status recorded as “" + a.status + "”." });
  }
  if (ci.activeBleeding) {
    alerts.push({ title: "Major Bleeding", reason: "Active bleeding reported by the field responder." });
  }
  if ((b.spo2 !== null && b.spo2 !== undefined && b.spo2 <= 91) || (b.rr !== null && b.rr !== undefined && (b.rr <= 8 || b.rr >= 25))) {
    const parts = [];
    if (b.spo2 !== null && b.spo2 !== undefined && b.spo2 <= 91) parts.push("SpO2 " + b.spo2 + "%");
    if (b.rr !== null && b.rr !== undefined && (b.rr <= 8 || b.rr >= 25)) parts.push("Respiratory rate " + b.rr + "/min");
    alerts.push({ title: "Respiratory Failure", reason: parts.join(" · ") + " indicates critical respiratory compromise." });
  }
  if ((qsofa !== null && qsofa >= 2) || (flags.infection && news2 !== null && news2 >= 5)) {
    const bits = [];
    if (qsofa !== null && qsofa >= 2) bits.push("qSOFA " + qsofa);
    if (flags.infection) bits.push("suspected infection");
    alerts.push({ title: "Possible Septic Shock", reason: bits.join(" + ") + " meet sepsis screening criteria." });
  }
  if (ci.bpSys !== null && ci.bpSys !== undefined && ci.bpSys <= 90) {
    alerts.push({ title: "Severe Hypotension", reason: "Systolic BP " + ci.bpSys + " mmHg (≤90)." + (ci.shockIndex ? " Shock index " + ci.shockIndex.toFixed(2) + "." : "") });
  }
  if (d.seizure) {
    alerts.push({ title: "Active Seizure", reason: "Seizure activity reported by the field responder." });
  }
  if (d.avpu && d.avpu !== "A") {
    alerts.push({ title: "Altered Mental Status", reason: "AVPU = " + (AVPU_LABEL[d.avpu] || d.avpu) + "." });
  }
  return alerts;
}

function highestTrigger(c, alerts) {
  const order = ["Airway Compromise", "Major Bleeding", "Respiratory Failure", "Possible Septic Shock", "Severe Hypotension", "Active Seizure", "Altered Mental Status"];
  for (const name of order) if (alerts.some(a => a.title === name)) return name;
  if (c.clinicalFlags && c.clinicalFlags.chestPain) return "Cardiac Emergency Risk";
  if (c.assessment && c.assessment.priorityLabel) return c.assessment.priorityLabel + " — clinical judgment";
  return "Routine Monitoring";
}

function reasonList(c) {
  const list = [...((c.assessment && c.assessment.findings) || [])];
  const si = c.circulation && c.circulation.shockIndex;
  if (si !== null && si !== undefined) list.push("Shock Index: " + si.toFixed(2));
  return list;
}

// =====================================================
// Rendering — queue (master)
// =====================================================

function buildQueueCardHtml(c) {
  const p = priorityOf(c);
  const meta = PRIORITY_META[p];
  const status = c.workflowStatus || "waiting";
  const news2 = (c.assessment && c.assessment.news2 !== null && c.assessment.news2 !== undefined) ? c.assessment.news2 : "--";
  const qsofa = (c.assessment && c.assessment.qsofa !== null && c.assessment.qsofa !== undefined) ? c.assessment.qsofa : "--";
  const isNew = !seenIds.has(c.bundleId);
  const isSelected = c.bundleId === selectedId;

  return (
    '<div class="queue-card risk-' + p + (isSelected ? " selected" : "") + (isNew ? " new-arrival" : "") + '" ' +
         'data-bundle-id="' + escAttr(c.bundleId) + '" role="button" tabindex="0">' +
      '<div class="qc-top"><span class="qc-priority-chip">' + meta.emoji + " " + meta.label.toUpperCase() + '</span></div>' +
      '<div class="qc-patient-id">' + escHtml(c.patientId || "--") + '</div>' +
      '<div class="qc-meta">' + escHtml(sexLabel(c.sex)) + " · " + escHtml(ageLabel(c)) + '</div>' +
      '<div class="qc-complaint-label">Chief Complaint</div>' +
      '<div class="qc-complaint-value">' + escHtml(c.chiefComplaint || "--") + '</div>' +
      '<div class="qc-scores"><span>NEWS2: <strong>' + news2 + '</strong></span><span>qSOFA: <strong>' + qsofa + '</strong></span></div>' +
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
    .sort((a, b) => new Date(b.completedAt || b.updatedAt || b.receivedAt) - new Date(a.completedAt || a.updatedAt || a.receivedAt));

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

function renderDetail(c) {
  const p = priorityOf(c);
  const meta = PRIORITY_META[p];
  const alerts = computeAlerts(c);
  const trigger = highestTrigger(c, alerts);
  const reasons = reasonList(c);

  const photoThumb = (c.images && c.images.length)
    ? '<img class="patient-photo-thumb" src="' + c.images[0].dataUrl + '" alt="Patient photo" data-viewer-index="0">'
    : '<div class="patient-photo-thumb" style="display:flex;align-items:center;justify-content:center;font-size:0.65rem;color:var(--text-secondary);text-align:center;">No Photo</div>';

  const html = `
    <section class="med-card">
      <h2 class="card-title">Patient Information</h2>
      <div class="card-body">
        <div class="patient-id-row">
          ${photoThumb}
          <div class="info-grid" style="flex:1;">
            ${infoItem("Patient ID", c.patientId)}
            ${infoItem("Name", c.name)}
            ${infoItem("Age", ageLabel(c))}
            ${infoItem("Sex", sexLabel(c.sex))}
          </div>
        </div>
        <div class="info-grid">
          ${infoItem("Weight", c.weightKg !== null && c.weightKg !== undefined ? c.weightKg + " kg" : null)}
          ${infoItem("Chief Complaint", c.chiefComplaint)}
          ${infoItem("Assessment Time", formatClock(c.assessmentTime) + " (" + formatRelative(c.assessmentTime) + ")")}
          ${infoItem("Responder Name", c.responderName)}
          ${infoItem("GPS Location", c.gps ? (c.gps.lat.toFixed(5) + ", " + c.gps.lng.toFixed(5)) : null)}
          ${infoItem("Arrival ETA", c.etaMinutes !== null && c.etaMinutes !== undefined ? c.etaMinutes + " min" : null)}
        </div>
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Airway</h2>
      <div class="card-body">
        <div class="info-grid">
          ${infoItem("Airway Status", c.airway && c.airway.status)}
        </div>
        ${c.airway && c.airway.notes ? '<p class="info-value" style="margin-top:10px;">' + escHtml(c.airway.notes) + '</p>' : ""}
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Breathing</h2>
      <div class="card-body">
        <div class="info-grid">
          ${infoItem("Respiratory Rate", c.breathing && c.breathing.rr !== null ? c.breathing.rr + " /min" : null)}
          ${infoItem("SpO₂", c.breathing && c.breathing.spo2 !== null ? c.breathing.spo2 + "%" : null)}
          ${infoItem("Supplemental Oxygen", c.breathing && c.breathing.onO2, { isBool: true })}
          ${infoItem("Breathing Effort", c.breathing && c.breathing.effort)}
          ${infoItem("Chest Expansion", c.breathing && c.breathing.chestExpansion)}
        </div>
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Circulation</h2>
      <div class="card-body">
        <div class="info-grid">
          ${infoItem("Heart Rate", c.circulation && c.circulation.pulse !== null ? c.circulation.pulse + " bpm" : null)}
          ${infoItem("Blood Pressure", (c.circulation && c.circulation.bpSys !== null && c.circulation.bpDia !== null) ? c.circulation.bpSys + "/" + c.circulation.bpDia + " mmHg" : null)}
          ${infoItem("Shock Index", c.circulation && c.circulation.shockIndex !== null && c.circulation.shockIndex !== undefined ? c.circulation.shockIndex.toFixed(2) : null)}
          ${infoItem("Capillary Refill", c.circulation && c.circulation.capillaryRefill)}
          ${infoItem("Skin Appearance", c.circulation && c.circulation.skinAppearance)}
          ${infoItem("Active Bleeding", c.circulation && c.circulation.activeBleeding, { isBool: true })}
          ${infoItem("Pulse Quality", c.circulation && c.circulation.pulseQuality)}
        </div>
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Disability</h2>
      <div class="card-body">
        <div class="info-grid">
          ${infoItem("AVPU", c.disability && c.disability.avpu ? (AVPU_LABEL[c.disability.avpu] || c.disability.avpu) : null)}
          ${infoItem("Pupil Response", c.disability && c.disability.pupilResponse)}
          ${infoItem("Seizure", c.disability && c.disability.seizure, { isBool: true })}
          ${infoItem("Blood Glucose", c.disability && c.disability.bloodGlucose !== null && c.disability.bloodGlucose !== undefined ? c.disability.bloodGlucose + " mg/dL" : null)}
        </div>
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Exposure</h2>
      <div class="card-body">
        <div class="info-grid">
          ${infoItem("Temperature", c.exposure && c.exposure.tempC !== null ? c.exposure.tempC.toFixed(1) + "°C" : null)}
          ${infoItem("Visible Trauma", c.exposure && c.exposure.visibleTrauma, { isBool: true })}
          ${infoItem("Burns", c.exposure && c.exposure.burns)}
          ${infoItem("Fracture", c.exposure && c.exposure.fracture)}
          ${infoItem("Mechanism of Injury", c.exposure && c.exposure.mechanismOfInjury)}
        </div>
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Medical History</h2>
      <div class="card-body">
        <div class="info-grid">
          ${infoItem("Known Conditions", (c.medicalHistory && c.medicalHistory.conditions && c.medicalHistory.conditions.length) ? c.medicalHistory.conditions.join(", ") : null)}
          ${infoItem("Current Medication", c.medicalHistory && c.medicalHistory.currentMedication)}
          ${infoItem("Drug Allergies", c.medicalHistory && c.medicalHistory.allergies)}
        </div>
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Emergency Notes</h2>
      <div class="card-body">
        <p class="info-value${c.notes ? "" : " muted"}">${c.notes ? escHtml(c.notes) : "No notes recorded"}</p>
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Uploaded Image</h2>
      <div class="card-body">
        ${(c.images && c.images.length)
          ? '<div class="image-gallery">' + c.images.map((img, i) =>
              '<div class="gallery-thumb" data-viewer-index="' + i + '"><img src="' + img.dataUrl + '" alt="Field image ' + (i + 1) + ' for ' + escAttr(c.patientId) + '"></div>'
            ).join("") + '</div>'
          : '<p class="info-value muted">No image captured</p>'}
      </div>
    </section>

    <section class="med-card assessment-card risk-${p}">
      <h2 class="card-title">Clinical Summary</h2>
      <div class="card-body">
        <div class="summary-priority-row">
          <span class="risk-dot risk-${p}"></span>
          <span class="summary-priority-badge">${meta.emoji} ${(c.assessment && c.assessment.priorityLabel) || meta.label.toUpperCase()}</span>
        </div>
        <div class="info-grid">
          ${infoItem("NEWS2", c.assessment && c.assessment.news2 !== null && c.assessment.news2 !== undefined ? c.assessment.news2 : null)}
          ${infoItem("qSOFA", c.assessment && c.assessment.qsofa !== null && c.assessment.qsofa !== undefined ? c.assessment.qsofa : null)}
          ${infoItem("Shock Index", c.circulation && c.circulation.shockIndex !== null && c.circulation.shockIndex !== undefined ? c.circulation.shockIndex.toFixed(2) : null)}
          ${infoItem("Triage Category", meta.emoji + " " + meta.color)}
        </div>
      </div>
    </section>

    <section class="med-card">
      <h2 class="card-title">Why This Patient Was Ranked</h2>
      <div class="card-body">
        <p class="info-value">Priority: <strong>${escHtml((c.assessment && c.assessment.priorityLabel) || "--")}</strong></p>
        <h3 class="explain-subheading" style="margin-top:12px;">Reason</h3>
        ${reasons.length ? '<ul class="reason-list">' + reasons.map(r => '<li>' + escHtml(r) + '</li>').join("") + '</ul>' : '<p class="info-value muted">No contributing findings recorded.</p>'}
        <p class="highest-trigger">Highest Trigger: ${escHtml(trigger)}</p>
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

function updateWorkflow(bundleId, newStatus) {
  if (typeof TgwStore === "undefined") return;
  TgwStore.updateCase(bundleId, (c) => {
    if (c.workflowStatus === newStatus) return c;
    c.workflowStatus = newStatus;
    const label = TIMELINE_LABEL[newStatus];
    if (label) c.timeline = [...(c.timeline || []), { ts: new Date().toISOString(), label }];
    if (newStatus === "completed") c.completedAt = new Date().toISOString();
    return c;
  });
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
// Event wiring
// =====================================================

function initTabs() {
  document.getElementById("tab-active").addEventListener("click", () => switchTab("active"));
  document.getElementById("tab-completed").addEventListener("click", () => switchTab("completed"));
}

function switchTab(tab) {
  activeTab = tab;
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
      const bundleId = $detailContent.dataset.bundleId;
      updateWorkflow(bundleId, wfBtn.dataset.workflow);
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
  document.getElementById("viewer-zoom-in").addEventListener("click", () => {
    viewerZoom = Math.min(4, viewerZoom + 0.25);
    applyViewerTransform();
  });
  document.getElementById("viewer-zoom-out").addEventListener("click", () => {
    viewerZoom = Math.max(0.5, viewerZoom - 0.25);
    applyViewerTransform();
  });
  document.getElementById("viewer-rotate").addEventListener("click", () => {
    viewerRotation = (viewerRotation + 90) % 360;
    applyViewerTransform();
  });
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

  if (typeof TgwStore !== "undefined") {
    render(TgwStore.loadCases());
    TgwStore.subscribe(render);
  }

  // Keep "Received: X min ago" / timeline clocks fresh without disrupting selection.
  setInterval(() => render(cases), 15000);
}

init();
