// Device-key gate: the screens a browser shows before the session canvas (unlock or set up the
// key, pick a PC, pair a PC, refusals). It only renders and calls back into dk-app.js, which holds
// the key, storage, PC list and sockets. Text goes in with textContent, never innerHTML.
// Styles: dk-gate.css, on the tokens in bubbleplay.css.

const GENERIC_ERROR = "Something went wrong.";
const PC_ID_PLACEHOLDER = "XXXX-XXXX-XXXX-XXXX";
const CODE_PLACEHOLDER = "XXXXX-XXXXX";
const PAIRING_TITLES = Object.freeze({
  connecting: "Asking the PC…",
  compare: "Compare the number",
  done: "Paired",
  denied: "Pairing declined",
  failed: "Pairing failed",
});
const PAIRING_DEFAULTS = Object.freeze({
  denied: "The PC declined. Nothing was saved.",
  failed: "Nothing was saved.",
});

const gates = new WeakMap();
let sequence = 0;
const nextId = prefix => `${prefix}-${++sequence}`;

/** The gate for one document. Creates the #dk-gate overlay under body on first use. */
export function mountGate(doc = document) {
  if (gates.has(doc)) return gates.get(doc);
  let root = doc.getElementById("dk-gate");
  if (!root) {
    root = doc.createElement("div");
    root.id = "dk-gate";
    root.setAttribute("role", "dialog");
    root.setAttribute("aria-modal", "true");
    root.hidden = true;
    doc.body.appendChild(root);
  }
  const gate = new Gate(doc, root);
  gates.set(doc, gate);
  return gate;
}

/** "Last used 5 min ago", "Last used 3 days ago", or the date for older use. */
function formatLastUsed(ms, now = Date.now()) {
  if (ms == null) return "Not used yet";
  const minutes = Math.floor((now - ms) / 60000);
  if (minutes < 1) return "Last used just now";
  if (minutes < 60) return `Last used ${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `Last used ${hours} h ago`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `Last used ${days} ${days === 1 ? "day" : "days"} ago`;
  return `Last used ${new Date(ms).toLocaleDateString(undefined, { day: "numeric", month: "short", year: "numeric" })}`;
}

/** "482913" -> "482 913". Anything that is not six digits is shown as given. */
function formatDigits(number) {
  const text = String(number ?? "");
  return /^\d{6}$/.test(text) ? `${text.slice(0, 3)} ${text.slice(3)}` : text;
}

/** One digit per word for screen readers: "482913" -> "4 8 2 9 1 3". */
function spokenDigits(number) {
  return String(number ?? "").split("").join(" ");
}

function messageOf(error) {
  return error && typeof error.message === "string" && error.message ? error.message : GENERIC_ERROR;
}

class Gate {
  constructor(doc, root) {
    this.doc = doc;
    this.root = root;
    this.token = 0;
    this.pending = false;
    this.actions = [];
    this.status = null;
    this.heading = null;
  }

  /** Key screen: unlock a protected key, or set up a browser with no key yet. */
  showKeyScreen({ mode, onUnlock, onCreate, onImport, onRemove } = {}) {
    if (mode !== "unlock") return this.showSetup({ onCreate, onImport });
    const panel = this.frame("Unlock this browser");
    const error = this.line("error");
    const form = this.el("form", "dk-gate-form");
    const passphrase = this.field("Passphrase", { type: "password", name: "passphrase", autocomplete: "current-password" });
    const unlock = this.button("Unlock", "primary", null, { submit: true });
    panel.append(this.el("p", "dk-gate-copy", "Your device key is protected with a passphrase."));
    form.append(passphrase.wrap, error, this.actionRow(unlock));
    form.addEventListener("submit", event => {
      event.preventDefault();
      this.run(() => onUnlock?.(passphrase.input.value), error, { busy: "Unlocking key…", refocus: passphrase.input });
    });
    panel.append(form);
    if (onRemove) panel.append(this.seam(), this.removal("Use a different key", "Remove the key stored in this browser? You can then create or import a key.", () => onRemove()));
    this.show(panel, passphrase.input);
  }

  showSetup({ onCreate, onImport }) {
    const panel = this.frame("Set up this browser");
    const error = this.line("error");
    panel.append(this.el("p", "dk-gate-copy", "BubblePlay connects with a device key stored in this browser. Create a new one, or import a key file you exported before."));
    const create = onCreate ? this.button("Create key", "primary", () => this.run(() => onCreate(), error)) : null;
    const importKey = onImport ? this.button("Import key file", "secondary", () => this.run(() => onImport(), error)) : null;
    panel.append(error, this.actionRow(create, importKey));
    this.show(panel);
  }

  /** PC list: pick a paired PC, rename or remove one, pair a new one, lock or forget this browser. */
  showPcList({ deviceId, pcs = [], onConnect, onRename, onRemove, onPair, onForgetBrowser, onLock } = {}) {
    const panel = this.frame("Your PCs");
    if (pcs.length) {
      const list = this.el("ul", "dk-gate-pcs");
      for (const pc of pcs) list.append(this.pcRow(pc, { onConnect, onRename, onRemove }));
      panel.append(list);
    } else {
      panel.append(this.el("p", "dk-gate-copy", "No PCs paired with this browser yet."));
    }
    if (onPair) panel.append(this.seam(), this.pairSection(pcs.length === 0, onPair));
    panel.append(this.seam(), this.footer(deviceId, { onLock, onForgetBrowser }));
    this.show(panel);
  }

  pcRow(pc, { onConnect, onRename, onRemove }) {
    const item = this.el("li", "dk-gate-pc");
    const main = this.el("button", "dk-gate-pc-main");
    main.type = "button";
    main.append(
      this.el("span", "dk-gate-pc-name", pc.name || pc.id),
      this.el("span", "dk-gate-mono dk-gate-pc-id", pc.id),
      this.el("span", "dk-gate-pc-used", formatLastUsed(pc.lastUsed)),
    );
    this.actions.push(main);
    main.addEventListener("click", () => onConnect?.(pc.id));

    let editor = null;
    const closeEditor = () => {
      editor?.remove();
      editor = null;
    };
    const tools = this.el("div", "dk-gate-pc-tools");
    if (onRename) {
      tools.append(this.button("Rename", "quiet", () => {
        closeEditor();
        const error = this.line("error");
        const form = this.el("form", "dk-gate-inline");
        const name = this.field("Name for this PC", { name: "pc-name", value: pc.name || "" });
        const save = this.button("Save name", "primary", null, { submit: true });
        form.append(name.wrap, error, this.actionRow(save, this.button("Cancel", "secondary", closeEditor)));
        form.addEventListener("submit", event => {
          event.preventDefault();
          this.run(() => onRename(pc.id, name.input.value), error, { refocus: name.input }).then(ok => {
            if (ok && editor === form) closeEditor();
          });
        });
        editor = form;
        item.append(form);
        name.input.focus();
      }));
    }
    if (onRemove) {
      tools.append(this.button("Remove", "quiet", () => {
        closeEditor();
        const { box, keep } = this.confirmBox(`Remove PC '${pc.name || pc.id}' (${pc.id})? This browser forgets its key; the PC keeps its own list.`, () => onRemove(pc.id), closeEditor);
        editor = box;
        item.append(box);
        keep.focus();
      }));
    }
    item.append(main, tools);
    return item;
  }

  /** Pair form. Raw strings go to onPair; the logic normalises and validates them. */
  pairSection(primary, onPair) {
    const section = this.el("section", "dk-gate-pair");
    section.append(this.el("h2", "dk-gate-heading-2", "Pair a PC"));
    const form = this.el("form", "dk-gate-form");
    const pc = this.field("PC ID", { name: "pc-id", placeholder: PC_ID_PLACEHOLDER });
    const code = this.field("Pairing code", { name: "pair-code", placeholder: CODE_PLACEHOLDER });
    const error = this.line("error");
    const pair = this.button("Pair", primary ? "primary" : "secondary", null, { submit: true });
    form.append(
      pc.wrap,
      code.wrap,
      this.el("p", "dk-gate-help", "On the PC: tray, Devices, Pair device. The code is valid for 5 minutes."),
      error,
      this.actionRow(pair),
    );
    form.addEventListener("submit", event => {
      event.preventDefault();
      this.run(() => onPair({ pc: pc.input.value, code: code.input.value }), error);
    });
    section.append(form);
    return section;
  }

  footer(deviceId, { onLock, onForgetBrowser }) {
    const box = this.el("div", "dk-gate-footer");
    if (deviceId) {
      const line = this.el("p", "dk-gate-foot", "This browser: ");
      line.append(this.el("span", "dk-gate-mono", deviceId));
      box.append(line);
    }
    const error = this.line("error");
    const lock = onLock ? this.button("Lock this browser", "secondary", () => this.run(() => onLock(), error)) : null;
    box.append(this.actionRow(lock), error);
    if (onForgetBrowser) {
      box.append(this.removal("Forget this browser", "Forget this browser? The relay will ask for approval again on the next visit. Your key and PC list stay in this browser.", () => onForgetBrowser()));
    }
    return box;
  }

  /** Pairing: connecting, the comparison number, done (name the PC), or a declined or failed end. */
  showPairing({ state, pcId, number, message, onCancel, onSave, onBack } = {}) {
    const known = Object.hasOwn(PAIRING_TITLES, state) ? state : "failed";
    const panel = this.frame(PAIRING_TITLES[known], { mono: pcId });
    const error = this.line("error");
    if (known === "connecting") {
      const progress = this.el("div", "dk-gate-progress");
      progress.setAttribute("aria-hidden", "true");
      progress.append(this.el("div", "dk-gate-progress-bar"));
      panel.append(progress);
    }
    if (known === "compare") {
      panel.append(this.el("p", "dk-gate-copy", "Check that the PC shows the same number, then choose Allow on the PC."));
      const digits = this.el("p", "dk-gate-number", formatDigits(number));
      digits.setAttribute("aria-label", spokenDigits(number));
      panel.append(digits);
    }
    if (known === "done") {
      panel.append(this.el("p", "dk-gate-copy", "The PC accepted this browser."));
      const form = this.el("form", "dk-gate-form");
      const name = this.field("Name for this PC", { name: "pc-name", value: pcId ?? "" });
      const save = this.button("Save and connect", "primary", null, { submit: true });
      form.append(name.wrap, error, this.actionRow(save));
      form.addEventListener("submit", event => {
        event.preventDefault();
        this.run(() => onSave?.(name.input.value), error);
      });
      panel.append(form);
    }
    if (known === "denied" || known === "failed") {
      panel.append(this.el("p", "dk-gate-copy", message || PAIRING_DEFAULTS[known]));
      panel.append(error, this.actionRow(onBack ? this.button("Back to PCs", "primary", () => this.run(() => onBack(), error)) : null));
    }
    if ((known === "connecting" || known === "compare") && onCancel) {
      panel.append(error, this.actionRow(this.button("Cancel", "secondary", () => this.run(() => onCancel(), error))));
    }
    this.show(panel);
  }

  /** Refusal: a recovery notice with a paw-pink seam on the left edge. */
  showRefusal({ title, body, actionLabel, onAction, secondaryLabel, onSecondary } = {}) {
    const panel = this.frame(title, { kind: "refusal" });
    const error = this.line("error");
    const actions = [];
    if (actionLabel && onAction) actions.push(this.button(actionLabel, "primary", () => this.run(() => onAction(), error)));
    if (secondaryLabel && onSecondary) actions.push(this.button(secondaryLabel, "secondary", () => this.run(() => onSecondary(), error)));
    panel.append(this.el("p", "dk-gate-copy", body ?? ""), error, this.actionRow(...actions));
    this.show(panel);
  }

  /** Busy: one status line on its own, for work that has no screen of its own yet. */
  showBusy(text) {
    this.actions = [];
    this.heading = null;
    const panel = this.el("section", "dk-gate-panel dk-gate-busy");
    panel.setAttribute("tabindex", "-1");
    this.status = this.line("status");
    this.status.textContent = text ?? "";
    panel.append(this.status);
    this.present(panel, panel, this.status.id);
  }

  /** Removes the overlay. The session canvas takes over. */
  hide() {
    this.token += 1;
    this.pending = false;
    this.actions = [];
    this.root.replaceChildren();
    this.root.hidden = true;
  }

  // ---- building blocks -------------------------------------------------------

  el(tag, className, text) {
    const node = this.doc.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  /** Starts a screen panel with its title. Call first: it resets the screen's controls. */
  frame(title, { kind = "", mono } = {}) {
    this.actions = [];
    const panel = this.el("section", kind ? `dk-gate-panel dk-gate-${kind}` : "dk-gate-panel");
    const heading = this.el("h1", "dk-gate-title", title);
    heading.id = nextId("dk-gate-title");
    heading.setAttribute("tabindex", "-1");
    this.heading = heading;
    panel.append(heading);
    if (mono) panel.append(this.el("p", "dk-gate-mono dk-gate-subtitle", mono));
    return panel;
  }

  show(panel, focusTarget = this.heading) {
    this.status = this.line("status");
    panel.append(this.status);
    this.present(panel, focusTarget, this.heading.id);
  }

  present(panel, focusTarget, labelId) {
    this.token += 1;
    this.pending = false;
    this.root.replaceChildren(panel);
    this.root.setAttribute("aria-labelledby", labelId);
    this.root.hidden = false;
    focusTarget.focus();
  }

  /** Status (role=status) or error (role=alert) line. Empty until text is set. */
  line(kind) {
    const node = this.el("p", kind === "error" ? "dk-gate-error" : "dk-gate-status");
    node.id = nextId("dk-gate-line");
    node.setAttribute("role", kind === "error" ? "alert" : "status");
    return node;
  }

  field(label, { type = "text", name = "", autocomplete = "off", placeholder = "", value = "" } = {}) {
    const wrap = this.el("div", "dk-gate-field");
    const caption = this.el("label", "dk-gate-label", label);
    const input = this.el("input", "dk-gate-input");
    input.id = nextId(`dk-gate-${name || "field"}`);
    caption.htmlFor = input.id;
    input.type = type;
    input.name = name;
    input.autocomplete = autocomplete;
    input.placeholder = placeholder;
    input.value = value;
    wrap.append(caption, input);
    return { wrap, input };
  }

  button(label, kind, onClick, { submit = false } = {}) {
    const node = this.el("button", `dk-gate-btn dk-gate-${kind}`, label);
    node.type = submit ? "submit" : "button";
    if (onClick) node.addEventListener("click", onClick);
    this.actions.push(node);
    return node;
  }

  actionRow(...buttons) {
    const row = this.el("div", "dk-gate-actions");
    row.append(...buttons.filter(Boolean));
    return row;
  }

  seam() {
    const node = this.el("div", "dk-gate-seam");
    node.setAttribute("aria-hidden", "true");
    return node;
  }

  /** A destructive action kept apart at the foot of a screen: the trigger opens an inline confirm. */
  removal(trigger, text, task) {
    const zone = this.el("div", "dk-gate-apart");
    const open = this.button(trigger, "quiet", null);
    open.addEventListener("click", () => {
      open.hidden = true;
      const { box, keep } = this.confirmBox(text, task, () => {
        box.remove();
        open.hidden = false;
        open.focus();
      });
      zone.append(box);
      keep.focus();
    });
    zone.append(open);
    return zone;
  }

  /**
   * Inline confirm naming the object. Returns the box and its Keep button; the caller attaches the
   * box and then focuses Keep, so Enter does not destroy anything.
   */
  confirmBox(text, task, onKeep) {
    const box = this.el("div", "dk-gate-confirm");
    const error = this.line("error");
    const confirm = this.button("Confirm", "danger", () => this.run(task, error));
    const keep = this.button("Keep", "secondary", onKeep);
    box.append(this.el("p", "dk-gate-copy", text), error, this.actionRow(confirm, keep));
    return { box, keep };
  }

  setDisabled(flag) {
    for (const node of this.actions) node.disabled = flag;
  }

  /**
   * Runs a callback while the screen is busy: its buttons are disabled and the status line says
   * what is happening. On reject the error message shows inline. Resolves true only when the
   * callback succeeded and the screen it started on is still showing.
   */
  run(task, errorLine, { busy = "", refocus = null } = {}) {
    if (this.pending) return Promise.resolve(false);
    const token = this.token;
    this.pending = true;
    this.setDisabled(true);
    errorLine.textContent = "";
    if (this.status) this.status.textContent = busy;
    return Promise.resolve()
      .then(task)
      .then(
        () => {
          if (token !== this.token) return false;
          this.settle();
          return true;
        },
        error => {
          if (token === this.token) {
            this.settle();
            errorLine.textContent = messageOf(error);
            refocus?.focus();
          }
          return false;
        },
      );
  }

  settle() {
    this.pending = false;
    this.setDisabled(false);
    if (this.status) this.status.textContent = "";
  }
}
