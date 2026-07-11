/* =====================================================
   Medical Data Capture - app.js
   Adaptive Clinical Decision Engine

   Sources:
   [1] Royal College of Physicians. NEWS2. London: RCP, 2017.
   [2] Singer M et al. Sepsis-3 (qSOFA). JAMA. 2016;315(8):801-810.
   [3] WHO. Emergency Triage Assessment and Treatment (ETAT).
       Geneva: WHO, 2005. ISBN: 9241546875.
   [4] Romig LE. JumpSTART Pediatric Triage.
       Prehosp Disaster Med. 2002;17(S1):S38.
   [5] Benson D et al. START triage. Newport Beach: Hoag, 1983.
   [6] ACS Committee on Trauma. ATLS 10th ed. Chicago: ACS, 2018.
   ===================================================== */

// --- DOM refs ---
const $form           = document.getElementById("capture-form");
const $patientName    = document.getElementById("patient-name");
const $patientAge     = document.getElementById("patient-age");
const $patientGender  = document.getElementById("patient-gender");
const $chiefComplaint = document.getElementById("chief-complaint");
const $patientId      = document.getElementById("patient-id");
const $patientWeight  = document.getElementById("patient-weight");
const $allergies      = document.getElementById("allergies");
const $notes          = document.getElementById("notes");

const $vitalRr        = document.getElementById("vital-rr");
const $vitalSpo2      = document.getElementById("vital-spo2");
const $vitalPulse     = document.getElementById("vital-pulse");
const $bpSys          = document.getElementById("vital-bp-sys");
const $bpDia          = document.getElementById("vital-bp-dia");
const $vitalTemp      = document.getElementById("vital-temp");
const $consciousness  = document.getElementById("consciousness");

const $toggleO2       = document.getElementById("toggle-o2");
const $toggleInfection= document.getElementById("toggle-infection");
const $toggleBreathing= document.getElementById("toggle-breathing");
const $toggleBleeding = document.getElementById("toggle-bleeding");
const $toggleChestPain= document.getElementById("toggle-chest-pain");
const $toggleTrauma   = document.getElementById("toggle-trauma");
const $togglePregnancy= document.getElementById("toggle-pregnancy");

const $imagePreviews  = document.getElementById("image-previews");
const $inputPhoto     = document.getElementById("input-take-photo");
const $inputUpload    = document.getElementById("input-upload-image");
const $btnTakePhoto   = document.getElementById("btn-take-photo");
const $btnUploadPhoto = document.getElementById("btn-upload-photo");

const $btnSend        = document.getElementById("btn-send");
const $sentList       = document.getElementById("sent-list");
const $sentEmpty      = document.getElementById("sent-empty");

const $assessmentCard  = document.getElementById("card-assessment-score");
const $assessmentProto = document.getElementById("assessment-protocol");
const $assessmentPrio  = document.getElementById("assessment-priority");
const $assessmentTriage= document.getElementById("assessment-triage");
const $assessmentAction= document.getElementById("assessment-action");
const $assessmentProgress = document.getElementById("assessment-progress");
const $priorityDot     = document.getElementById("priority-dot");
const $triageDot       = document.getElementById("triage-dot");
const $mentalStatusGroup = document.getElementById("mental-status-group");
const $msConfusion     = document.getElementById("ms-confusion");
const $explainSection  = document.getElementById("assessment-explain");
const $explainProtocol = document.getElementById("explain-protocol");
const $explainFindings = document.getElementById("explain-findings");
const $explainRules    = document.getElementById("explain-rules");
const $explainFinal    = document.getElementById("explain-final");
const $explainSources  = document.getElementById("explain-sources");

// --- State ---
let selectedImages = [];
let conditions = [];
const sentItems = [];
let currentStream = null;

const PATIENT_ID_KEY = "tgw-patient-counter";
const PATIENT_ID_BASE = 1023;

const SOURCES =
  "[1] RCP NEWS2 (2017)  [2] Sepsis-3 qSOFA, JAMA 2016  [3] WHO ETAT, ISBN 9241546875  [4] JumpSTART, Prehosp Disaster Med 2002  [5] START, Hoag Hospital 1983  [6] ATLS 10th ed, ACS 2018";

const vitalConfigs = [
  { input: $vitalRr,   errorId: "error-vital-rr",     min: 4,   max: 80,  label: "Respiratory rate",   decimals: 0 },
  { input: $vitalSpo2, errorId: "error-vital-spo2",   min: 50,  max: 100, label: "Oxygen saturation",  decimals: 0 },
  { input: $vitalPulse,errorId: "error-vital-pulse",   min: 20,  max: 260, label: "Pulse rate",        decimals: 0 },
  { input: $bpSys,     errorId: "error-vital-bp-sys",  min: 40,  max: 300, label: "Systolic BP",        decimals: 0 },
  { input: $bpDia,     errorId: "error-vital-bp-dia",  min: 20,  max: 200, label: "Diastolic BP",       decimals: 0 },
];

// --- Patient ID auto-gen ---
function nextPatientId() {
  let n = PATIENT_ID_BASE;
  try {
    const stored = localStorage.getItem(PATIENT_ID_KEY);
    if (stored) n = parseInt(stored, 10) || PATIENT_ID_BASE;
    n += 1;
    localStorage.setItem(PATIENT_ID_KEY, String(n));
  } catch (_) { n += 1; }
  return "P-" + n;
}

// --- Init ---
function init() {
  $form.addEventListener("submit", handleSubmit);
  $patientId.value = nextPatientId();

  const liveInputs = [
    $patientName, $patientAge, $patientGender, $chiefComplaint, $patientWeight, $allergies,
    $vitalRr, $vitalSpo2, $vitalPulse, $bpSys, $bpDia, $vitalTemp, $consciousness, $notes
  ];
  liveInputs.forEach(el => el.addEventListener("input", () => { updateSendButton(); updateAssessment(); }));

  $patientGender.addEventListener("change", updatePregnancyToggle);

  // Mental status segmented buttons
  $mentalStatusGroup.addEventListener("click", (e) => {
    const btn = e.target.closest(".mental-status-btn");
    if (!btn || btn.hidden) return;
    $mentalStatusGroup.querySelectorAll(".mental-status-btn").forEach(b => b.classList.remove("selected"));
    btn.classList.add("selected");
    $consciousness.value = btn.dataset.value;
    updateAssessment();
    updateSendButton();
  });

  // Age-based mental status button switching (adult shows New Confusion, pediatric hides it)
  $patientAge.addEventListener("change", () => {
    const adult = isAdult();
    $msConfusion.hidden = !adult;
    if (!adult && $consciousness.value === "C") {
      $consciousness.value = "";
      $mentalStatusGroup.querySelectorAll(".mental-status-btn").forEach(b => b.classList.remove("selected"));
    }
    updateAssessment();
    updateSendButton();
  });

  vitalConfigs.forEach(cfg => setupVitalValidator(cfg));
  setupBpValidator();

  [$toggleO2, $toggleInfection, $toggleBreathing, $toggleBleeding, $toggleChestPain, $toggleTrauma, $togglePregnancy]
    .forEach(t => t.addEventListener("click", () => { if (t.disabled) return; toggleSwitch(t); updateAssessment(); updateSendButton(); }));

  document.getElementById("conditions-chips").addEventListener("click", (e) => {
    const chip = e.target.closest(".chip");
    if (!chip) return;
    const cond = chip.dataset.condition;
    chip.classList.toggle("selected");
    if (chip.classList.contains("selected")) {
      if (!conditions.includes(cond)) conditions.push(cond);
    } else {
      conditions = conditions.filter(c => c !== cond);
    }
  });

  $btnTakePhoto.addEventListener("click", openCamera);
  $btnUploadPhoto.addEventListener("click", () => $inputUpload.click());
  $inputPhoto.addEventListener("change", handleImageSelect);
  $inputUpload.addEventListener("change", handleImageSelect);

  document.getElementById("btn-camera-close").addEventListener("click", closeCamera);
  document.getElementById("btn-camera-capture").addEventListener("click", capturePhoto);
  document.getElementById("btn-camera-retry").addEventListener("click", openCamera);
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeCamera(); });
  document.addEventListener("visibilitychange", () => { if (document.hidden) closeCamera(); });

  updateSendButton();
  updateAssessment();
}

// --- Toggle switch ---
function toggleSwitch(el) {
  const current = el.getAttribute("aria-checked") === "true";
  el.setAttribute("aria-checked", String(!current));
}
function isToggleOn(el) { return el.getAttribute("aria-checked") === "true"; }

function updatePregnancyToggle() {
  const isMale = $patientGender.value === "M";
  $togglePregnancy.disabled = isMale;
  $togglePregnancy.classList.toggle("toggle-disabled", isMale);
  if (isMale) $togglePregnancy.setAttribute("aria-checked", "false");
}

// --- Temperature (Celsius) ---
function getTempCelsius() {
  const val = parseFloat($vitalTemp.value);
  return isNaN(val) ? null : val;
}

// --- Validation ---
const vitalErrorEls = {};
function setupVitalValidator(cfg) {
  const $err = document.getElementById(cfg.errorId);
  vitalErrorEls[cfg.input.id] = $err;
  const handler = () => validateVital(cfg.input, $err, cfg);
  cfg.input.addEventListener("input", handler);
  cfg.input.addEventListener("blur", handler);
}
function setupBpValidator() {
  const $err = document.getElementById("error-vital-bp-sys");
  vitalErrorEls["vital-bp-sys"] = $err;
  vitalErrorEls["vital-bp-dia"] = $err;
  const handler = () => validateBp();
  $bpSys.addEventListener("input", handler);
  $bpDia.addEventListener("input", handler);
  $bpSys.addEventListener("blur", handler);
  $bpDia.addEventListener("blur", handler);
}
function showVitalError($err, msg) { if ($err) { $err.textContent = msg; $err.hidden = !msg; } }
function setInvalid($input, isInvalid) { $input.classList.toggle("is-invalid", isInvalid); }
function validateVital($input, $err, cfg) {
  const raw = $input.value.trim();
  if (raw === "") { showVitalError($err, ""); setInvalid($input, false); return true; }
  const num = Number(raw);
  let valid = true;
  if (!Number.isFinite(num)) { showVitalError($err, cfg.label + " must be a number."); valid = false; }
  else if (cfg.decimals === 0 && !Number.isInteger(num)) { showVitalError($err, cfg.label + " must be a whole number."); valid = false; }
  else if (num < cfg.min || num > cfg.max) { showVitalError($err, cfg.label + " must be between " + cfg.min + " and " + cfg.max + "."); valid = false; }
  else showVitalError($err, "");
  setInvalid($input, !valid);
  return valid;
}
function validateBp() {
  const $err = vitalErrorEls["vital-bp-sys"];
  const sysRaw = $bpSys.value.trim(), diaRaw = $bpDia.value.trim();
  if (sysRaw === "" && diaRaw === "") { showVitalError($err, ""); setInvalid($bpSys, false); setInvalid($bpDia, false); return true; }
  const sys = Number(sysRaw), dia = Number(diaRaw);
  let valid = true;
  if ((sysRaw !== "" && !Number.isFinite(sys)) || (diaRaw !== "" && !Number.isFinite(dia))) { showVitalError($err, "Blood pressure must be numbers."); valid = false; }
  else if (sysRaw !== "" && (sys < 40 || sys > 300)) { showVitalError($err, "Systolic must be between 40 and 300."); valid = false; }
  else if (diaRaw !== "" && (dia < 20 || dia > 200)) { showVitalError($err, "Diastolic must be between 20 and 200."); valid = false; }
  else if (sysRaw !== "" && diaRaw !== "" && sys <= dia) { showVitalError($err, "Systolic must be greater than diastolic."); valid = false; }
  else showVitalError($err, "");
  setInvalid($bpSys, !valid && sysRaw !== "");
  setInvalid($bpDia, !valid && diaRaw !== "");
  return valid;
}
function allVitalsValid() {
  const checks = vitalConfigs.map(cfg => validateVital(cfg.input, vitalErrorEls[cfg.input.id], cfg));
  checks.push(validateBp());
  return checks.every(Boolean);
}

function allRequiredFilled() {
  return !!(
    $patientName.value.trim() &&
    $patientAge.value.trim() &&
    $patientGender.value &&
    $chiefComplaint.value.trim() &&
    $vitalRr.value.trim() &&
    $vitalPulse.value.trim() &&
    $vitalTemp.value.trim() &&
    $consciousness.value
  );
}
function updateSendButton() { $btnSend.disabled = !(allRequiredFilled() && allVitalsValid()); }

// =====================================================
// ADAPTIVE CLINICAL DECISION ENGINE
// =====================================================

function num(id) { const v = parseInt(document.getElementById(id).value, 10); return isNaN(v) ? null : v; }
function fnum(id) { const v = parseFloat(document.getElementById(id).value); return isNaN(v) ? null : v; }

function getAge() { return $patientAge.value || null; }
function isAdult() { const a = getAge(); return a === "Adult" || a === "Senior"; }

// --- NEWS2 score [1] ---
function calcNEWS2(findings, rules) {
  const rr = num("vital-rr"), spo2 = num("vital-spo2"), onO2 = isToggleOn($toggleO2);
  const pulse = num("vital-pulse"), sbp = num("vital-bp-sys"), avpu = $consciousness.value;
  const tempC = getTempCelsius();

  let score = 0, params = 0;
  const add = (pts, finding) => { score += pts; if (finding) findings.push(finding); };

  if (rr !== null) {
    params++;
    if (rr <= 8) add(3, "Respiratory rate " + rr + "/min (very low)");
    else if (rr >= 25) add(3, "Respiratory rate " + rr + "/min (very high)");
    else if (rr >= 21) add(2, "Respiratory rate " + rr + "/min (elevated)");
    else if (rr <= 11) add(1, "Respiratory rate " + rr + "/min (low)");
  }

  if (spo2 !== null) {
    params++;
    if (spo2 <= 91) add(3, "SpO2 " + spo2 + "% (critically low)");
    else if (spo2 <= 93) add(2, "SpO2 " + spo2 + "% (low)");
    else if (spo2 <= 95) add(1, "SpO2 " + spo2 + "% (below target)");
  }

  if (onO2) add(2, "On supplemental oxygen");

  if (pulse !== null) {
    params++;
    if (pulse <= 40) add(3, "Pulse " + pulse + " bpm (very low)");
    else if (pulse >= 131) add(3, "Pulse " + pulse + " bpm (very high)");
    else if (pulse >= 111) add(2, "Pulse " + pulse + " bpm (elevated)");
    else if (pulse <= 50) add(1, "Pulse " + pulse + " bpm (low)");
    else if (pulse >= 91) add(1, "Pulse " + pulse + " bpm (slightly elevated)");
  }

  if (sbp !== null) {
    params++;
    if (sbp <= 90) add(3, "Systolic BP " + sbp + " mmHg (very low)");
    else if (sbp >= 220) add(3, "Systolic BP " + sbp + " mmHg (very high)");
    else if (sbp <= 100) add(2, "Systolic BP " + sbp + " mmHg (low)");
    else if (sbp <= 110) add(1, "Systolic BP " + sbp + " mmHg (slightly low)");
  }

  if (avpu) {
    params++;
    if (avpu !== "A") add(3, "Altered consciousness (" + avpuLabel(avpu) + ")");
  }

  if (tempC !== null) {
    params++;
    if (tempC <= 35.0) add(3, "Temperature " + tempC.toFixed(1) + "C (hypothermia)");
    else if (tempC >= 39.1) add(2, "Temperature " + tempC.toFixed(1) + "C (fever)");
    else if (tempC <= 36.0) add(1, "Temperature " + tempC.toFixed(1) + "C (low)");
    else if (tempC >= 38.1) add(1, "Temperature " + tempC.toFixed(1) + "C (elevated)");
  }

  const hasAll = params >= 4 && rr !== null && pulse !== null && tempC !== null && avpu;
  return { score: hasAll ? score : null, hasAll };
}

function avpuLabel(v) {
  return { A: "Alert", C: "New Confusion", V: "Responds to Voice", P: "Responds to Pain", U: "Unresponsive" }[v] || v;
}

// --- qSOFA [2] ---
function calcQSOFA(findings, rules) {
  const rr = num("vital-rr"), sbp = num("vital-bp-sys"), avpu = $consciousness.value;
  let score = 0, params = 0;
  if (rr !== null) { params++; if (rr >= 22) { score++; findings.push("Respiratory rate " + rr + "/min (>=22, qSOFA)"); } }
  if (sbp !== null) { params++; if (sbp <= 100) { score++; findings.push("Systolic BP " + sbp + " mmHg (<=100, qSOFA)"); } }
  if (avpu) { params++; if (avpu !== "A") { score++; findings.push("Altered mentation (" + avpuLabel(avpu) + ", qSOFA)"); } }
  const hasAll = params >= 2;
  return { score: hasAll ? score : null, hasAll };
}

// --- START [5] + ATLS [6] rules (adult) ---
function evalSTARTRules(findings, rules) {
  const rr = num("vital-rr"), sbp = num("vital-bp-sys"), avpu = $consciousness.value;
  if (avpu === "U") { rules.push("ATLS: Unresponsive patient (immediate airway protection) [6]"); findings.push("Patient unresponsive"); }
  if (rr !== null && rr > 30) { rules.push("START: RR > 30 (immediate) [5]"); }
  if (sbp !== null && sbp < 90) { rules.push("START: No effective perfusion, SBP < 90 (immediate) [5]"); }
  if (avpu && avpu !== "A" && avpu !== "C") { rules.push("START: Unable to obey commands (immediate) [5]"); }
  if (isToggleOn($toggleBleeding)) { rules.push("ATLS: Active hemorrhage (life-threatening) [6]"); findings.push("Active bleeding reported"); }
  if (isToggleOn($toggleChestPain)) { rules.push("ATLS: Chest pain (potential cardiac emergency) [6]"); findings.push("Chest pain reported"); }
  if (isToggleOn($toggleTrauma)) { rules.push("ATLS: Trauma/injury present [6]"); findings.push("Trauma reported"); }
  if (isToggleOn($toggleBreathing)) { rules.push("ATLS: Difficulty breathing [6]"); findings.push("Difficulty breathing reported"); }
}

// --- Pediatric normal ranges (category-adjusted) [3][4] ---
function pedsRanges(category) {
  switch (category) {
    case "Infant":   return { rrMin: 30, rrMax: 60, pulseMin: 100, pulseMax: 160, sbpMin: 60, sbpMax: 90 };
    case "Toddler":  return { rrMin: 24, rrMax: 40, pulseMin: 90,  pulseMax: 150, sbpMin: 70, sbpMax: 100 };
    case "Child":    return { rrMin: 20, rrMax: 30, pulseMin: 80,  pulseMax: 130, sbpMin: 80, sbpMax: 110 };
    case "Teenager": return { rrMin: 12, rrMax: 20, pulseMin: 60, pulseMax: 110, sbpMin: 100, sbpMax: 130 };
    default:         return { rrMin: 20, rrMax: 30, pulseMin: 80,  pulseMax: 130, sbpMin: 80, sbpMax: 110 };
  }
}

// --- WHO ETAT [3] + JumpSTART [4] rules (pediatric) ---
function evalPediatricRules(category, findings, rules) {
  const rr = num("vital-rr"), spo2 = num("vital-spo2"), pulse = num("vital-pulse");
  const sbp = num("vital-bp-sys"), avpu = $consciousness.value;
  const ranges = pedsRanges(category);

  if (avpu === "U") { rules.push("WHO ETAT: Coma (emergency sign) [3]"); findings.push("Patient unresponsive"); }
  if (avpu === "P") { rules.push("JumpSTART: Responds to pain only (immediate) [4]"); findings.push("Responds to pain, not commands"); }

  if (rr !== null) {
    if (rr < ranges.rrMin || rr > ranges.rrMax) findings.push("Respiratory rate " + rr + "/min outside age-adjusted range (" + ranges.rrMin + "-" + ranges.rrMax + ")");
    if (rr < 15 || rr > 45) rules.push("JumpSTART: RR " + rr + " outside 15-45 (immediate) [4]");
  }

  if (spo2 !== null && spo2 < 90) { rules.push("WHO ETAT: SpO2 < 90% (severe respiratory distress) [3]"); findings.push("SpO2 " + spo2 + "% (critical)"); }

  if (sbp !== null && sbp < ranges.sbpMin) { rules.push("WHO ETAT: Shock signs, SBP " + sbp + " below age-adjusted minimum [3]"); findings.push("SBP " + sbp + " mmHg (low for age)"); }
  if (pulse !== null && (pulse < ranges.pulseMin || pulse > ranges.pulseMax)) findings.push("Pulse " + pulse + " bpm outside age-adjusted range (" + ranges.pulseMin + "-" + ranges.pulseMax + ")");

  if (isToggleOn($toggleBleeding)) { rules.push("ATLS: Active hemorrhage (life-threatening) [6]"); findings.push("Active bleeding reported"); }
  if (isToggleOn($toggleBreathing)) { rules.push("WHO ETAT: Respiratory distress reported [3]"); findings.push("Difficulty breathing reported"); }
  if (isToggleOn($toggleTrauma)) { rules.push("ATLS: Trauma/injury present [6]"); findings.push("Trauma reported"); }
  if (isToggleOn($toggleChestPain)) { rules.push("ATLS: Chest pain (potential cardiac emergency) [6]"); findings.push("Chest pain reported"); }

  if (category === "Infant") { rules.push("WHO ETAT: Infant under 1 year (priority sign) [3]"); findings.push("Infant (under 1 year)"); }
  if (avpu === "C") { rules.push("WHO ETAT: Altered consciousness (priority sign) [3]"); findings.push("Confused"); }
}

// --- Unified triage output ---
const TRIAGE = {
  immediate: { label: "Immediate", color: "critical", action: "Immediate life-saving intervention. Transfer now." },
  delayed:   { label: "Delayed",   color: "moderate", action: "Urgent medical review required. Monitor closely." },
  minor:     { label: "Minor",     color: "stable",   action: "Routine care. Continue monitoring." },
};

const PRIORITY = {
  critical: { label: "Critical", color: "critical" },
  high:     { label: "High Risk", color: "high" },
  moderate: { label: "Moderate", color: "moderate" },
  stable:   { label: "Stable", color: "stable" },
};

// --- Required fields progress tracker ---
const REQUIRED_FIELDS = [
  { id: "patient-name", label: "Patient Name" },
  { id: "patient-age", label: "Age Category" },
  { id: "patient-gender", label: "Gender" },
  { id: "chief-complaint", label: "Chief Complaint" },
  { id: "vital-rr", label: "Respiratory Rate" },
  { id: "vital-pulse", label: "Pulse Rate" },
  { id: "vital-temp", label: "Body Temperature" },
  { id: "consciousness", label: "Mental Status" },
];

function getRequiredProgress() {
  let completed = 0;
  const missing = [];
  for (const f of REQUIRED_FIELDS) {
    const el = document.getElementById(f.id);
    const val = el ? el.value.trim() : "";
    if (val) { completed++; } else { missing.push(f.label); }
  }
  return { completed, total: REQUIRED_FIELDS.length, missing };
}

function calculateAssessment() {
  const age = getAge();
  if (age === null) return null;

  const progress = getRequiredProgress();
  const protocolName = isAdult() ? "Adult" : "Pediatric";
  const protocolDetail = isAdult()
    ? "NEWS2 + qSOFA + START + ATLS"
    : "WHO ETAT + JumpSTART + ATLS";

  // Do NOT calculate triage/priority until all required fields are complete
  if (progress.completed < progress.total) {
    return {
      protocol: protocolName,
      protocolDetail: protocolDetail,
      waiting: true,
      progress: progress,
      priority: null,
      triage: null,
      findings: [],
      rules: [],
      hasData: false,
    };
  }

  const findings = [];
  const rules = [];
  let triage = null;
  let priority = null;

  if (isAdult()) {

    const news2 = calcNEWS2(findings, rules);
    const qsofa = calcQSOFA(findings, rules);
    evalSTARTRules(findings, rules);

    if (qsofa.score !== null && qsofa.score >= 2) {
      triage = TRIAGE.immediate;
      rules.push("qSOFA score " + qsofa.score + " >= 2 (high-risk sepsis criteria) [2]");
    }
    if (news2.score !== null && news2.score >= 7) {
      triage = TRIAGE.immediate;
      rules.push("NEWS2 score " + news2.score + " >= 7 (escalation threshold) [1]");
    }

    if (!triage && (rules.some(r => r.includes("ATLS: Unresponsive")) || rules.some(r => r.includes("ATLS: Active hemorrhage")))) {
      triage = TRIAGE.immediate;
    }

    if (!triage) {
      if (rules.some(r => r.includes("START:"))) { triage = TRIAGE.immediate; }
      if (!triage && news2.score !== null && news2.score >= 5) { triage = TRIAGE.delayed; rules.push("NEWS2 score " + news2.score + " (urgent review) [1]"); }
      if (!triage && qsofa.score !== null && qsofa.score >= 1) { triage = TRIAGE.delayed; rules.push("qSOFA score " + qsofa.score + " (monitor) [2]"); }
      if (!triage && news2.score !== null && news2.score >= 3) { triage = TRIAGE.delayed; rules.push("NEWS2 score " + news2.score + " (observation required) [1]"); }
      if (!triage && isToggleOn($toggleChestPain)) { triage = TRIAGE.delayed; }
      if (!triage && news2.score !== null && news2.score <= 2) { triage = TRIAGE.minor; }
    }

    if (triage === TRIAGE.immediate) priority = PRIORITY.critical;
    else if (triage === TRIAGE.delayed) {
      const ruleCount = rules.filter(r => !r.includes("NEWS2") && !r.includes("qSOFA")).length;
      priority = ruleCount >= 2 ? PRIORITY.high : PRIORITY.moderate;
    } else if (triage === TRIAGE.minor) {
      priority = PRIORITY.stable;
    }

    if (news2.score !== null) findings.unshift("NEWS2 score: " + news2.score);
    if (qsofa.score !== null) findings.splice(1, 0, "qSOFA score: " + qsofa.score);

  } else {
    evalPediatricRules(age, findings, rules);

    if (rules.some(r => r.includes("immediate") || r.includes("Coma") || r.includes("hemorrhage") || r.includes("JumpSTART: RR") || r.includes("SpO2") || r.includes("Shock"))) {
      triage = TRIAGE.immediate;
    }

    if (!triage) {
      if (rules.some(r => r.includes("priority sign") || r.includes("ATLS: Trauma") || r.includes("ATLS: Chest"))) {
        triage = TRIAGE.delayed;
      }
      if (!triage && findings.length > 0) { triage = TRIAGE.delayed; }
      if (!triage) { triage = TRIAGE.minor; }
    }

    if (triage === TRIAGE.immediate) priority = PRIORITY.critical;
    else if (triage === TRIAGE.delayed) priority = rules.length >= 2 ? PRIORITY.high : PRIORITY.moderate;
    else priority = PRIORITY.stable;
  }

  return {
    protocol: protocolName,
    protocolDetail: protocolDetail,
    priority: priority,
    triage: triage,
    findings: findings,
    rules: rules,
    hasData: findings.length > 0 || rules.length > 0,
  };
}

// --- Shock index (pulse / systolic BP), stored on the case record ---
function calcShockIndex() {
  const pulse = num("vital-pulse"), sbp = num("vital-bp-sys");
  if (pulse === null || sbp === null || sbp <= 0) return null;
  return pulse / sbp;
}

// --- Update assessment card UI ---
function updateAssessment() {
  const result = calculateAssessment();

  $assessmentCard.classList.remove("risk-stable", "risk-moderate", "risk-high", "risk-critical");
  $priorityDot.className = "risk-dot";
  $triageDot.className = "risk-dot";

  if (!result) {
    $assessmentProto.textContent = "Select age category";
    $assessmentPrio.textContent = "--";
    $assessmentTriage.textContent = "--";
    $assessmentAction.textContent = "Fill in patient information and vital signs to generate assessment.";
    $assessmentProgress.hidden = true;
    $explainSection.hidden = true;
    return;
  }

  $assessmentProto.textContent = result.protocol;

  if (result.waiting) {
    $assessmentPrio.textContent = "--";
    $assessmentTriage.textContent = "--";
    $assessmentAction.textContent = "Waiting for required clinical information";
    $assessmentProgress.hidden = false;
    $assessmentProgress.textContent = result.progress.completed + " of " + result.progress.total + " required fields completed"
      + (result.progress.missing.length > 0 ? " - Missing: " + result.progress.missing.join(", ") : "");
    $explainSection.hidden = true;
    return;
  }

  $assessmentProgress.hidden = true;

  if (result.priority) {
    $assessmentPrio.textContent = result.priority.label;
    $assessmentCard.classList.add("risk-" + result.priority.color);
    $priorityDot.classList.add("risk-" + result.priority.color);
    $assessmentPrio.classList.remove("score-anim");
    void $assessmentPrio.offsetWidth;
    $assessmentPrio.classList.add("score-anim");
  } else {
    $assessmentPrio.textContent = "--";
  }

  if (result.triage) {
    $assessmentTriage.textContent = result.triage.label;
    $triageDot.classList.add("risk-" + result.triage.color);
    $assessmentTriage.classList.remove("score-anim");
    void $assessmentTriage.offsetWidth;
    $assessmentTriage.classList.add("score-anim");
    $assessmentAction.textContent = result.triage.action;
  } else {
    $assessmentTriage.textContent = "--";
    $assessmentAction.textContent = "Complete vital signs for full assessment.";
  }

  // Explainability
  if (result.hasData) {
    $explainSection.hidden = false;
    $explainProtocol.textContent = "Protocol: " + result.protocol + " (" + result.protocolDetail + ")";

    $explainFindings.innerHTML = "";
    result.findings.forEach(f => {
      const li = document.createElement("li");
      li.textContent = f;
      $explainFindings.appendChild(li);
    });

    $explainRules.innerHTML = "";
    result.rules.forEach(r => {
      const li = document.createElement("li");
      li.textContent = r;
      $explainRules.appendChild(li);
    });

    $explainFinal.textContent = result.triage ? result.triage.label : "Not yet determined";
    $explainSources.textContent = SOURCES;
  } else {
    $explainSection.hidden = true;
  }
}

// =====================================================
// CAMERA + IMAGE HANDLING
// =====================================================

async function openCamera() {
  const $cameraOverlay = document.getElementById("camera-overlay");
  const $cameraVideo = document.getElementById("camera-video");
  hideCameraError();
  closeCamera();
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    showCameraError(!window.isSecureContext
      ? "Camera requires a secure page (HTTPS or localhost). Open this page via http://localhost to use the camera."
      : "This browser does not support camera access.");
    return;
  }
  try {
    const stream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: { ideal: "environment" } }, audio: false });
    currentStream = stream;
    $cameraVideo.srcObject = stream;
    $cameraOverlay.hidden = false;
    await $cameraVideo.play();
  } catch (err) {
    console.error("Camera access denied or unavailable", err);
    closeCamera();
    showCameraError(cameraErrorMessage(err));
  }
}

function cameraErrorMessage(err) {
  const name = err && err.name ? err.name : "";
  switch (name) {
    case "NotAllowedError": case "SecurityError": return "Camera permission denied. Allow camera access in your browser settings and try again.";
    case "NotFoundError": case "DevicesNotFoundError": return "No camera found on this device.";
    case "NotReadableError": case "TrackStartError": return "The camera is in use by another app. Close it and try again.";
    case "OverconstrainedError": return "No camera matched the requested settings.";
    default: return "Could not start the camera. Please check permissions and try again.";
  }
}

function showCameraError(message) {
  const $cameraOverlay = document.getElementById("camera-overlay");
  const $cameraVideo = document.getElementById("camera-video");
  const $cameraControls = document.querySelector(".camera-controls");
  const $cameraError = document.getElementById("camera-error");
  const $cameraErrorText = document.getElementById("camera-error-text");
  $cameraVideo.hidden = true;
  if ($cameraControls) $cameraControls.style.display = "none";
  if ($cameraErrorText) $cameraErrorText.textContent = message;
  if ($cameraError) $cameraError.hidden = false;
  $cameraOverlay.hidden = false;
}

function hideCameraError() {
  const $cameraVideo = document.getElementById("camera-video");
  const $cameraControls = document.querySelector(".camera-controls");
  const $cameraError = document.getElementById("camera-error");
  $cameraVideo.hidden = false;
  if ($cameraControls) $cameraControls.style.display = "";
  if ($cameraError) $cameraError.hidden = true;
}

function closeCamera() {
  const $cameraOverlay = document.getElementById("camera-overlay");
  const $cameraVideo = document.getElementById("camera-video");
  if (currentStream) { currentStream.getTracks().forEach(t => t.stop()); currentStream = null; }
  $cameraVideo.srcObject = null;
  hideCameraError();
  $cameraOverlay.hidden = true;
}

function capturePhoto() {
  const $cameraVideo = document.getElementById("camera-video");
  const $cameraCanvas = document.getElementById("camera-canvas");
  if (!currentStream || !$cameraVideo.videoWidth || !$cameraVideo.videoHeight) return;
  $cameraCanvas.width = $cameraVideo.videoWidth;
  $cameraCanvas.height = $cameraVideo.videoHeight;
  const ctx = $cameraCanvas.getContext("2d");
  ctx.drawImage($cameraVideo, 0, 0, $cameraCanvas.width, $cameraCanvas.height);
  $cameraCanvas.toBlob(blob => {
    if (!blob) return;
    const file = new File([blob], "captured_photo_" + Date.now() + ".webp", { type: "image/webp" });
    addImage(file);
    closeCamera();
  }, "image/webp", 0.9);
}

function handleImageSelect(e) {
  const files = Array.from(e.target.files);
  if (!files.length) return;
  e.target.value = "";
  files.forEach(file => reencodeToWebp(file, (webpFile) => addImage(webpFile || file)));
}

function reencodeToWebp(file, cb) {
  const url = URL.createObjectURL(file);
  const img = new Image();
  img.onload = () => {
    URL.revokeObjectURL(url);
    const canvas = document.createElement("canvas");
    canvas.width = img.naturalWidth || img.width;
    canvas.height = img.naturalHeight || img.height;
    try {
      const ctx = canvas.getContext("2d");
      ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
      canvas.toBlob(blob => {
        if (!blob) { cb(null); return; }
        const name = file.name.replace(/\.[^.]+$/, "") + ".webp";
        cb(new File([blob], name, { type: "image/webp" }));
      }, "image/webp", 0.9);
    } catch (err) { console.error("WebP re-encode failed", err); cb(null); }
  };
  img.onerror = () => { URL.revokeObjectURL(url); cb(null); };
  img.src = url;
}

function addImage(file) {
  selectedImages.push(file);
  renderImagePreviews();
  updateAssessment();
  updateSendButton();
}

function removeImage(idx) {
  selectedImages.splice(idx, 1);
  renderImagePreviews();
  updateAssessment();
  updateSendButton();
}

function renderImagePreviews() {
  $imagePreviews.innerHTML = "";
  selectedImages.forEach((file, idx) => {
    const thumb = document.createElement("div");
    thumb.className = "image-thumb";
    const url = URL.createObjectURL(file);
    thumb.innerHTML = '<img src="' + url + '" alt="Photo ' + (idx + 1) + '">';
    const removeBtn = document.createElement("button");
    removeBtn.className = "image-thumb-remove";
    removeBtn.setAttribute("aria-label", "Remove image " + (idx + 1));
    removeBtn.innerHTML = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"></line><line x1="6" y1="6" x2="18" y2="18"></line></svg>';
    removeBtn.addEventListener("click", () => removeImage(idx));
    thumb.appendChild(removeBtn);
    $imagePreviews.appendChild(thumb);
  });
}

// =====================================================
// SUBMISSION + SENT LIST
// =====================================================

// --- Resize + re-encode a File to a compressed WebP data URL for storage ---
function fileToResizedDataUrl(file, maxDim, quality) {
  return new Promise((resolve) => {
    const url = URL.createObjectURL(file);
    const img = new Image();
    img.onload = () => {
      URL.revokeObjectURL(url);
      const scale = Math.min(1, maxDim / Math.max(img.naturalWidth || img.width, img.naturalHeight || img.height));
      const w = Math.max(1, Math.round((img.naturalWidth || img.width) * scale));
      const h = Math.max(1, Math.round((img.naturalHeight || img.height) * scale));
      const canvas = document.createElement("canvas");
      canvas.width = w; canvas.height = h;
      try {
        const ctx = canvas.getContext("2d");
        ctx.drawImage(img, 0, 0, w, h);
        resolve(canvas.toDataURL("image/webp", quality));
      } catch (err) {
        console.error("image resize failed", err);
        resolve(null);
      }
    };
    img.onerror = () => { URL.revokeObjectURL(url); resolve(null); };
    img.src = url;
  });
}

function buildCaseRecord(bundleId, assessment) {
  const nowIso = new Date().toISOString();
  return {
    bundleId,
    patientId: $patientId.value.trim(),
    name: $patientName.value.trim(),
    ageYears: null,
    ageCategory: $patientAge.value || null,
    sex: $patientGender.value,
    weightKg: fnum("patient-weight"),
    chiefComplaint: $chiefComplaint.value.trim(),
    responderName: null,
    gps: null,
    etaMinutes: null,
    assessmentTime: nowIso,
    receivedAt: nowIso,

    airway: {
      status: null,
      notes: null,
    },
    breathing: {
      rr: num("vital-rr"), spo2: num("vital-spo2"), onO2: isToggleOn($toggleO2),
      difficulty: isToggleOn($toggleBreathing),
      effort: null,
      chestExpansion: null,
    },
    circulation: {
      pulse: num("vital-pulse"), bpSys: num("vital-bp-sys"), bpDia: num("vital-bp-dia"),
      shockIndex: calcShockIndex(),
      capillaryRefill: null,
      skinAppearance: null,
      activeBleeding: isToggleOn($toggleBleeding),
      pulseQuality: null,
    },
    disability: {
      avpu: $consciousness.value || null,
      pupilResponse: null,
      seizure: false,
      bloodGlucose: null,
    },
    exposure: {
      tempC: getTempCelsius(),
      tempUnit: "C",
      visibleTrauma: isToggleOn($toggleTrauma),
      burns: "None",
      fracture: "None",
      mechanismOfInjury: null,
    },

    medicalHistory: {
      conditions: [...conditions],
      currentMedication: null,
      allergies: $allergies.value.trim() || null,
    },
    notes: $notes.value.trim() || null,
    clinicalFlags: {
      infection: isToggleOn($toggleInfection),
      chestPain: isToggleOn($toggleChestPain),
      pregnancy: isToggleOn($togglePregnancy),
    },

    images: [],

    assessment: {
      protocol: assessment ? assessment.protocol : null,
      priority: assessment && assessment.priority ? assessment.priority.color : null,
      priorityLabel: assessment && assessment.priority ? assessment.priority.label : null,
      triage: assessment && assessment.triage ? assessment.triage.color : null,
      triageLabel: assessment && assessment.triage ? assessment.triage.label : null,
      findings: assessment ? assessment.findings : [],
      rules: assessment ? assessment.rules : [],
      news2: (assessment && assessment.protocol === "Adult") ? calcNEWS2([], []).score : null,
      qsofa: (assessment && assessment.protocol === "Adult") ? calcQSOFA([], []).score : null,
    },

    workflowStatus: "waiting",
    timeline: [{ ts: nowIso, label: "Assessment Received" }],

    createdAt: nowIso,
    updatedAt: nowIso,
  };
}

function handleSubmit(e) {
  e.preventDefault();
  if (!allRequiredFilled() || !allVitalsValid()) return;

  const assessment = calculateAssessment();
  $btnSend.disabled = true;
  $btnSend.querySelector("span").textContent = "Submitting...";
  $btnSend.classList.add("sending");

  const bundleId = (typeof TgwStore !== "undefined") ? TgwStore.uuid() : String(Date.now());
  const caseRecord = buildCaseRecord(bundleId, assessment);
  const imagesSnapshot = [...selectedImages];

  const patient = {
    id: Date.now(),
    patientId: $patientId.value.trim(),
    name: $patientName.value.trim(),
    age: $patientAge.value,
    gender: $patientGender.value,
    chiefComplaint: $chiefComplaint.value.trim(),
    weight: $patientWeight.value.trim() || null,
    allergies: $allergies.value.trim() || null,
    conditions: [...conditions],
    notes: $notes.value.trim() || null,
    clinical: {
      infection: isToggleOn($toggleInfection),
      breathing: isToggleOn($toggleBreathing),
      bleeding: isToggleOn($toggleBleeding),
      chestPain: isToggleOn($toggleChestPain),
      trauma: isToggleOn($toggleTrauma),
      pregnancy: isToggleOn($togglePregnancy),
    },
    images: selectedImages.map(f => f.name),
    vitals: {
      rr: num("vital-rr"), spo2: num("vital-spo2"), onO2: isToggleOn($toggleO2),
      pulse: num("vital-pulse"), bpSys: num("vital-bp-sys"), bpDia: num("vital-bp-dia"),
      temp: fnum("vital-temp"), tempUnit: "C", consciousness: $consciousness.value,
    },
    protocol: assessment ? assessment.protocol : null,
    priority: assessment && assessment.priority ? assessment.priority.color : null,
    priorityLabel: assessment && assessment.priority ? assessment.priority.label : null,
    triage: assessment && assessment.triage ? assessment.triage.color : null,
    triageLabel: assessment && assessment.triage ? assessment.triage.label : null,
    status: "preparing",
    timestamp: new Date().toISOString(),
  };

  sentItems.unshift(patient);
  updateSentList();

  // Fire-and-forget: resize/encode images then push the full case to the
  // shared hospital queue. Runs alongside the local transmit simulation
  // below; the two are independent (this is the hospital-side channel,
  // not the RaptorQ path the "sent" list is simulating).
  Promise.all(imagesSnapshot.map(f => fileToResizedDataUrl(f, 1280, 0.8)))
    .then(dataUrls => {
      caseRecord.images = imagesSnapshot.map((f, i) => ({ name: f.name, dataUrl: dataUrls[i] })).filter(img => img.dataUrl);
      if (typeof TgwStore !== "undefined") TgwStore.upsertCase(caseRecord);
    })
    .catch(err => console.error("failed to prepare case images", err));

  // Bridge to the REAL send path: POST the vitals to the local field agent
  // (`tgw-field serve`), which seals + RaptorQ-encodes + sends over UDP to the gateway and
  // returns the true delivery outcome. If no bridge is reachable (e.g. the UI is served from a
  // plain static server), fall back to the local transmit simulation so the standalone demo
  // still works.
  patient.status = "transmitting";
  updateSentList();

  const restoreButton = () => {
    $btnSend.querySelector("span").textContent = "Submit Patient Data";
    $btnSend.classList.remove("sending");
  };

  sendToBackend(patient)
    .then(result => {
      if (result.rejected) {
        patient.status = "error";
        patient.statusDetail = result.message;
      } else {
        patient.status = result.delivered ? "delivered" : "stuck";
        if (result.short_id) patient.bundleId = result.short_id;
      }
      updateSentList();
      restoreButton();
      if (!result.rejected) clearForm();
    })
    .catch(() => {
      // Bridge unreachable — keep the old local simulation so the standalone UI still demos.
      const recovered = Math.random() < 0.15;
      setTimeout(() => {
        patient.status = recovered ? "recovered" : "delivered";
        updateSentList();
        restoreButton();
        clearForm();
      }, 1600);
    });
}

// Bridge a capture to the local field agent's real UDP send path (`tgw-field serve`).
// Resolves with the agent's JSON result ({ short_id, state, delivered }) on a reachable bridge —
// including a { rejected, message } shape for a 4xx (e.g. vitals outside the input bounds).
// Rejects ONLY when the bridge is unreachable, so the caller can fall back to the simulation.
async function sendToBackend(patient) {
  const v = patient.vitals || {};
  const body = {
    patient: patient.patientId,
    device: "field-ui",
    performer: "field-worker",
    bp_sys: v.bpSys != null ? v.bpSys : null,
    bp_dia: v.bpDia != null ? v.bpDia : null,
    spo2:   v.spo2  != null ? v.spo2  : null,
    pulse:  v.pulse != null ? v.pulse : null,
  };
  let res;
  try {
    res = await fetch("/api/capture", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch (e) {
    const err = new Error("field bridge unreachable");
    err.bridgeUnreachable = true;
    throw err;
  }
  if (!res.ok) {
    const message = await res.text().catch(() => res.statusText);
    return { rejected: true, delivered: false, message };
  }
  return res.json();
}

function clearForm() {
  $patientId.value = nextPatientId();
  $patientName.value = "";
  $patientAge.value = "";
  $patientGender.value = "";
  $chiefComplaint.value = "";
  $patientWeight.value = "";
  $allergies.value = "";
  $notes.value = "";
  $vitalRr.value = "";
  $vitalSpo2.value = "";
  $vitalPulse.value = "";
  $bpSys.value = "";
  $bpDia.value = "";
  $vitalTemp.value = "";
  $consciousness.value = "";

  $mentalStatusGroup.querySelectorAll(".mental-status-btn").forEach(b => b.classList.remove("selected"));

  [$toggleO2, $toggleInfection, $toggleBreathing, $toggleBleeding, $toggleChestPain, $toggleTrauma, $togglePregnancy]
    .forEach(t => t.setAttribute("aria-checked", "false"));

  conditions = [];
  document.querySelectorAll(".chip").forEach(c => c.classList.remove("selected"));

  selectedImages = [];
  renderImagePreviews();

  updateSendButton();
  updateAssessment();
}

// --- Sent list (sorted by triage severity) ---
const triageOrder = { critical: 0, high: 1, moderate: 2, stable: 3 };

function updateSentList() {
  if (sentItems.length === 0) {
    $sentEmpty.style.display = "block";
    $sentList.innerHTML = "";
    return;
  }
  $sentEmpty.style.display = "none";

  const sorted = [...sentItems].sort((a, b) => {
    const ra = a.priority ? triageOrder[a.priority] : 4;
    const rb = b.priority ? triageOrder[b.priority] : 4;
    if (ra !== rb) return ra - rb;
    return new Date(b.timestamp) - new Date(a.timestamp);
  });

  $sentList.innerHTML = "";
  for (const item of sorted) {
    const li = document.createElement("li");
    li.className = "patient-card";
    if (item.priority) li.classList.add("risk-" + item.priority);

    const riskBadge = item.priority
      ? '<span class="risk-badge risk-' + item.priority + '"><span class="risk-dot risk-' + item.priority + '"></span>' + escHtml(item.priorityLabel) + '</span>'
      : '<span class="risk-badge"><span class="risk-dot"></span>Not assessed</span>';

    const statusChip = statusChipHtml(item.status);
    const triageText = item.triageLabel ? item.triageLabel : "--";
    const protoText = item.protocol || "--";
    const ageText = item.age || "--";

    li.innerHTML =
      '<div class="patient-card-top">' +
        '<span class="patient-name">' + escHtml(item.name) + '</span>' +
        riskBadge +
      '</div>' +
      '<div class="patient-meta">' +
        '<span>' + ageText + '</span>' +
        '<span>' + escHtml(item.gender === "F" ? "Female" : item.gender === "M" ? "Male" : "Other") + '</span>' +
        '<span>' + escHtml(item.chiefComplaint) + '</span>' +
      '</div>' +
      '<div class="patient-scores">' +
        '<span class="patient-score-item">Protocol: <strong>' + escHtml(protoText) + '</strong></span>' +
        '<span class="patient-score-item">Triage: <strong>' + escHtml(triageText) + '</strong></span>' +
      '</div>' +
      '<div class="patient-bottom">' +
        statusChip +
        '<span class="patient-time">' + formatTime(item.timestamp) + '</span>' +
      '</div>';

    $sentList.appendChild(li);
  }
}

function statusChipHtml(status) {
  const labels = { preparing: "Preparing", transmitting: "Transmitting", delivered: "Delivered", recovered: "Recovered via FEC", failed: "Failed", stuck: "Kept (STUCK)", error: "Rejected" };
  return '<span class="status-chip status-' + status + '"><span class="status-dot"></span>' + (labels[status] || status) + '</span>';
}

function formatTime(iso) {
  const d = new Date(iso);
  return d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
}

function escHtml(str) {
  if (str == null) return "";
  return String(str).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

// =====================================================
// DEMO AUTO-FILL
// =====================================================

function setToggle(el, on) {
  el.setAttribute("aria-checked", on ? "true" : "false");
}

function setMentalStatus(value) {
  $consciousness.value = value;
  $mentalStatusGroup.querySelectorAll(".mental-status-btn").forEach(b => {
    b.classList.toggle("selected", b.dataset.value === value);
  });
}

const DEMOS = {
  1: {
    name: "John Smith", age: "Adult", gender: "M",
    complaint: "Mild ankle sprain",
    rr: 16, spo2: 99, pulse: 74, bpSys: 120, bpDia: 80, temp: 36.8,
    o2: false, mental: "A",
    infection: false, breathing: false, bleeding: false, chestPain: false, trauma: true, pregnancy: false,
  },
  2: {
    name: "Robert Chen", age: "Senior", gender: "M",
    complaint: "Severe chest pain and difficulty breathing",
    rr: 32, spo2: 84, pulse: 138, bpSys: 82, bpDia: 48, temp: 39.5,
    o2: true, mental: "V",
    infection: false, breathing: true, bleeding: false, chestPain: true, trauma: false, pregnancy: false,
  },
  3: {
    name: "Emma Davis", age: "Child", gender: "F",
    complaint: "Difficulty breathing",
    rr: 48, spo2: 85, pulse: 170, bpSys: "", bpDia: "", temp: 39.6,
    o2: false, mental: "V",
    infection: true, breathing: true, bleeding: false, chestPain: false, trauma: false, pregnancy: false,
  },
};

function loadDemo(n) {
  const d = DEMOS[n];
  if (!d) return;

  clearForm();

  $patientName.value = d.name;
  $patientAge.value = d.age;
  $patientGender.value = d.gender;
  $chiefComplaint.value = d.complaint;

  $vitalRr.value = d.rr;
  $vitalSpo2.value = d.spo2;
  $vitalPulse.value = d.pulse;
  $bpSys.value = d.bpSys;
  $bpDia.value = d.bpDia;
  $vitalTemp.value = d.temp;

  setToggle($toggleO2, d.o2);
  setMentalStatus(d.mental);

  setToggle($toggleInfection, d.infection);
  setToggle($toggleBreathing, d.breathing);
  setToggle($toggleBleeding, d.bleeding);
  setToggle($toggleChestPain, d.chestPain);
  setToggle($toggleTrauma, d.trauma);
  setToggle($togglePregnancy, d.pregnancy);

  $msConfusion.hidden = !isAdult();
  if (!isAdult() && d.mental === "C") {
    $consciousness.value = "";
    $mentalStatusGroup.querySelectorAll(".mental-status-btn").forEach(b => b.classList.remove("selected"));
  }

  updatePregnancyToggle();

  updateAssessment();
  updateSendButton();
}

init();
