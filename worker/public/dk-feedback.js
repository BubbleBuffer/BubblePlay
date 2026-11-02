// "Report a problem" for the browser viewer: a small dialog (menu item and Ctrl+Shift+F) that
// builds a report from what the person wrote plus recent redacted log lines, and stores it in
// IndexedDB (database "bp-logs", store "feedback") through dk-log.js. Phase B sends stored
// reports to the PC the next time this browser connects. No screenshot is taken here: the
// browser page shows the remote stream, which a report must never carry.
// Report shape: docs/logging.md (version 1). Text goes in with textContent, never innerHTML.
import { browserDeviceName } from "./dk-session.js";

export const FEEDBACK_KINDS = Object.freeze([
  { value: "bug", label: "Bug" },
  { value: "complaint", label: "Complaint" },
  { value: "idea", label: "Idea" },
  { value: "other", label: "Other" },
]);
export const FEEDBACK_TEXT_MAX = 4000;
export const SAVED_TEXT = "Saved. It is sent to your PC the next time you connect.";
export const SAVED_MEMORY_TEXT = "Saved for now. It is sent to your PC the next time you connect, if this tab stays open.";
const LOG_LINES = 200;
const ERROR_LINES = 20;

function randomId(cryptoObject = globalThis.crypto) {
  if (typeof cryptoObject?.randomUUID === "function") return cryptoObject.randomUUID();
  return `r-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

/** The current screen: the device-key gate (unlock, PC list, pairing) or the session. */
export function currentRoute(doc = globalThis.document, loc = globalThis.location) {
  const gate = doc?.getElementById?.("dk-gate");
  if (gate && !gate.hidden) return "gate";
  return new URLSearchParams(loc?.search ?? "").get("pc") ? "session" : "list";
}

/**
 * Builds the report object. Every string passes through the logger's redact(); the log lines
 * are already redacted. Throws when the logger is missing, so nothing unredacted is stored.
 */
export function buildReport({
  text, kind = "other", log = globalThis.BPLog, nav = globalThis.navigator, loc = globalThis.location,
  doc = globalThis.document, now = () => new Date(), id = randomId(), locale,
} = {}) {
  if (!log?.redact) throw new Error("The logger is not available.");
  const redact = value => log.redact(String(value ?? ""));
  const context = log.context?.() ?? {};
  const kinds = FEEDBACK_KINDS.map(item => item.value);
  return {
    version: 1,
    id,
    created: now().toISOString(),
    text: redact(String(text ?? "").slice(0, FEEDBACK_TEXT_MAX)),
    kind: kinds.includes(kind) ? kind : "other",
    app: { version: "web", build: context.build ? redact(context.build) : null, commit: null },
    platform: redact(nav?.userAgentData?.platform || nav?.platform || ""),
    device: { id: context.device ?? null, name: redact(browserDeviceName(nav)), model: null },
    pc: context.pc ?? null,
    session: context.session ?? null,
    route: redact(currentRoute(doc, loc)),
    locale: redact(locale ?? nav?.language ?? ""),
    logs: log.recent(LOG_LINES),
    errors: log.errors(ERROR_LINES),
  };
}

const FOCUSABLE = "textarea, select, button";

function element(doc, tag, props = {}, ...children) {
  const node = doc.createElement(tag);
  for (const [name, value] of Object.entries(props)) {
    if (name === "text") node.textContent = value;
    else if (name === "class") node.className = value;
    else node.setAttribute(name, value);
  }
  for (const child of children) node.appendChild(child);
  return node;
}

/** The dialog. `doc`, `log` and the rest are test seams; production uses the page globals. */
export function createFeedback({ doc = globalThis.document, log = globalThis.BPLog, nav, loc, now, id } = {}) {
  let root = null, dialog = null, field = null, kindSelect = null, status = null, send = null, cancel = null;
  let returnFocus = null, saved = false, busy = false;

  function build() {
    root = element(doc, "div", { id: "bp-feedback", class: "bp-feedback-scrim" });
    root.hidden = true;
    dialog = element(doc, "div", { class: "bp-feedback", role: "dialog", "aria-modal": "true", "aria-labelledby": "bp-feedback-title", tabindex: "-1" });
    const title = element(doc, "h2", { id: "bp-feedback-title", text: "Report a problem" });
    field = element(doc, "textarea", { id: "bp-feedback-text", rows: "5", maxlength: String(FEEDBACK_TEXT_MAX), "aria-describedby": "bp-feedback-help" });
    kindSelect = element(doc, "select", { id: "bp-feedback-kind" });
    for (const item of FEEDBACK_KINDS) kindSelect.appendChild(element(doc, "option", { value: item.value, text: item.label }));
    kindSelect.value = "bug";
    const help = element(doc, "p", { id: "bp-feedback-help", class: "bp-feedback-help",
      text: "Adds the time and recent activity from this browser. Passwords and keys are removed first." });
    status = element(doc, "p", { id: "bp-feedback-status", class: "bp-feedback-status", role: "status", "aria-live": "polite" });
    send = element(doc, "button", { id: "bp-feedback-send", type: "button", class: "bp-feedback-primary", text: "Send" });
    cancel = element(doc, "button", { id: "bp-feedback-cancel", type: "button", text: "Cancel" });
    dialog.append(
      title,
      element(doc, "label", { for: "bp-feedback-text", text: "What happened?" }), field,
      element(doc, "label", { for: "bp-feedback-kind", text: "Kind" }), kindSelect,
      help, status,
      element(doc, "div", { class: "bp-feedback-actions" }, send, cancel),
    );
    root.appendChild(dialog);
    doc.body.appendChild(root);
    // Typed text belongs to this dialog: keep it away from the remote input handlers.
    for (const type of ["keydown", "keyup", "keypress", "paste", "copy", "cut"]) {
      dialog.addEventListener(type, event => event.stopPropagation());
    }
    dialog.addEventListener("keydown", onDialogKey);
    send.addEventListener("click", submit);
    cancel.addEventListener("click", close);
    root.addEventListener("click", event => { if (event.target === root) close(); });
  }

  function focusables() {
    return [...dialog.querySelectorAll(FOCUSABLE)].filter(node => !node.disabled && !node.hidden);
  }

  function onDialogKey(event) {
    if (event.key === "Escape") {
      event.preventDefault();
      close();
      return;
    }
    if (event.key !== "Tab") return;
    const items = focusables();
    if (!items.length) { event.preventDefault(); return; }
    const first = items[0], last = items[items.length - 1];
    const active = doc.activeElement;
    if (event.shiftKey && (active === first || active === dialog)) { event.preventDefault(); last.focus(); }
    else if (!event.shiftKey && active === last) { event.preventDefault(); first.focus(); }
  }

  function open() {
    if (!root) build();
    if (!root.hidden) return;
    returnFocus = doc.activeElement && doc.activeElement !== doc.body ? doc.activeElement : null;
    saved = false; busy = false;
    field.value = ""; field.readOnly = false; field.disabled = false; kindSelect.disabled = false;
    kindSelect.value = "bug";
    send.disabled = false; send.hidden = false;
    cancel.textContent = "Cancel";
    status.textContent = log?.redact ? "" : "Reports are not available in this browser.";
    if (!log?.redact) send.disabled = true;
    root.hidden = false;
    field.focus();
  }

  function close() {
    if (!root || root.hidden) return;
    root.hidden = true;
    const target = returnFocus;
    returnFocus = null;
    target?.focus?.();
  }

  async function submit() {
    if (busy || saved) return;
    const text = field.value.trim();
    if (!text) {
      status.textContent = "Describe what happened first.";
      field.focus();
      return;
    }
    busy = true;
    send.disabled = true;
    try {
      const report = buildReport({ text, kind: kindSelect.value, log, nav, loc, doc, now, id });
      const result = await log.saveReport(report);
      saved = true;
      field.readOnly = true; kindSelect.disabled = true;
      send.hidden = true;
      cancel.textContent = "Close";
      status.textContent = result?.persisted === false ? SAVED_MEMORY_TEXT : SAVED_TEXT;
      cancel.focus();
      try { log.info("feedback", "report saved", { id: report.id, kind: report.kind, persisted: result?.persisted !== false }); } catch { /* ignore */ }
    } catch (error) {
      status.textContent = "This report could not be saved. Try again.";
      send.disabled = false;
      try { log.warn("feedback", "report not saved", { cause: String(error?.message ?? error).slice(0, 160) }); } catch { /* ignore */ }
    } finally {
      busy = false;
    }
  }

  /** Ctrl+Shift+F, in the capture phase so a running session does not forward it to the PC. */
  function onShortcut(event) {
    if (!(event.ctrlKey && event.shiftKey && !event.altKey && !event.metaKey)) return;
    if (event.code !== "KeyF" && String(event.key).toLowerCase() !== "f") return;
    event.preventDefault();
    event.stopImmediatePropagation?.();
    event.stopPropagation();
    open();
  }

  return { open, close, submit, onShortcut, isOpen: () => Boolean(root && !root.hidden) };
}

/** Installs the dialog and the shortcut for this page; exposes it as globalThis.BPFeedback. */
export function installFeedback(doc = globalThis.document) {
  const feedback = createFeedback({ doc });
  doc.addEventListener("keydown", feedback.onShortcut, true);
  globalThis.BPFeedback = feedback;
  return feedback;
}
