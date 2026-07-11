/* =====================================================
   Shared patient-case store — field-ui <-> hospital-ui
   localStorage for persistence, BroadcastChannel (with a
   storage-event fallback) so both static apps stay in sync
   when served from the same origin. No backend involved:
   this is a same-machine demo bridge, not the RaptorQ path.
   ===================================================== */
(function (global) {
  "use strict";

  const STORAGE_KEY = "tgw-hospital-cases-v1";
  const CHANNEL_NAME = "tgw-hospital-cases";

  const channel = (typeof BroadcastChannel !== "undefined")
    ? new BroadcastChannel(CHANNEL_NAME)
    : null;

  const localListeners = new Set();

  function uuid() {
    if (typeof crypto !== "undefined" && crypto.randomUUID) return crypto.randomUUID();
    return "case-" + Date.now().toString(36) + "-" + Math.random().toString(36).slice(2, 10);
  }

  function nowIso() {
    return new Date().toISOString();
  }

  function loadCases() {
    try {
      const raw = localStorage.getItem(STORAGE_KEY);
      if (!raw) return [];
      const parsed = JSON.parse(raw);
      return Array.isArray(parsed) ? parsed : [];
    } catch (err) {
      console.error("tgw-store: failed to load cases", err);
      return [];
    }
  }

  function saveCases(cases) {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(cases));
    } catch (err) {
      console.error("tgw-store: failed to save cases", err);
    }
    notify(cases);
  }

  function notify(cases) {
    if (channel) {
      try { channel.postMessage({ type: "sync", at: Date.now() }); } catch (_) { /* ignore */ }
    }
    localListeners.forEach(cb => {
      try { cb(cases); } catch (err) { console.error("tgw-store listener error", err); }
    });
  }

  function upsertCase(caseRecord) {
    const cases = loadCases();
    const idx = cases.findIndex(c => c.bundleId === caseRecord.bundleId);
    if (idx === -1) cases.push(caseRecord);
    else cases[idx] = caseRecord;
    saveCases(cases);
    return cases;
  }

  function updateCase(bundleId, updaterFn) {
    const cases = loadCases();
    const idx = cases.findIndex(c => c.bundleId === bundleId);
    if (idx === -1) return cases;
    const updated = updaterFn({ ...cases[idx] });
    updated.updatedAt = nowIso();
    cases[idx] = updated;
    saveCases(cases);
    return cases;
  }

  function subscribe(callback) {
    localListeners.add(callback);

    const onMessage = () => callback(loadCases());
    const onStorage = (e) => { if (e.key === STORAGE_KEY) callback(loadCases()); };

    if (channel) channel.addEventListener("message", onMessage);
    global.addEventListener("storage", onStorage);

    return function unsubscribe() {
      localListeners.delete(callback);
      if (channel) channel.removeEventListener("message", onMessage);
      global.removeEventListener("storage", onStorage);
    };
  }

  global.TgwStore = { loadCases, saveCases, upsertCase, updateCase, subscribe, uuid, nowIso };
})(window);
