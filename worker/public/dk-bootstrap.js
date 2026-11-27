// Page-start trace and debug attempt id. A file, not an inline script, so the page CSP needs no script-src 'unsafe-inline'.
(() => {
  "use strict";
  const search = new URLSearchParams(location.search);
  const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
  const suppliedAttemptId = search.get("debugAttempt") || "";
  const attemptId = uuid.test(suppliedAttemptId)
    ? suppliedAttemptId.toLowerCase()
    : crypto.randomUUID();
  const state = {
    attemptId,
    seq: 0,
  };
  const MAX_FIELDS = 16;
  const MAX_TEXT = 512;
  const normalizeDetail = detail => Object.fromEntries(Object.entries(detail || {}).slice(0, MAX_FIELDS).map(([key, value]) => {
    if (Array.isArray(value)) return [key, value.slice(0, MAX_FIELDS).map(item => typeof item === "string" ? item.slice(0, MAX_TEXT) : item)];
    if (typeof value === "string") return [key, value.slice(0, MAX_TEXT)];
    return [key, value ?? null];
  }));
  // Page-start events go to the local browser log (dk-log.js, loaded first). Errors and
  // rejections are captured by that script itself.
  const trace = (stage, detail = {}) => {
    try {
      window.BPLog?.info("trace", stage, { attempt: state.attemptId, seq: ++state.seq, ...normalizeDetail(detail) });
    } catch { /* tracing never breaks the page */ }
  };
  window.__bubbleplayDebugTrace = {
    attemptId: state.attemptId,
    trace,
  };
  // navigate | reload | back_forward | prerender: tells a manual reload from a recovery restart.
  const navigationType = () => {
    try {
      const type = window.performance?.getEntriesByType?.("navigation")?.[0]?.type;
      return typeof type === "string" ? type : null;
    } catch { return null; }
  };
  trace("bootstrap-inline", {
    pathname: location.pathname,
    navigation: navigationType(),
    transport: search.get("transport") || "auto",
    decoderPath: search.get("decoderPath") || "native",
  });
})();
