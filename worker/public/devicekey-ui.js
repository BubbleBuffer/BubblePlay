// Device-key screens for the browser viewer: the settings entry and the create, import, export,
// change-passphrase and remove dialogs. Key format and crypto: devicekey.js. Storage:
// devicekey-store.js. Spec: docs/architecture/device-keys.md. A key is mandatory: dk-app.js unlocks
// it before anything connects and opens the create/import dialogs from here through the returned
// createKey/importKey.
// The unlocked key is never kept. Passphrases are read from inputs, and the dialog that held them
// is removed when it closes (JavaScript strings cannot be wiped, so references are dropped).
import { createKeyFile, isOtherFormatKeyFileText, parseKeyFile, rewrapKeyFile } from "./devicekey.js";
import { createDeviceKeyStore } from "./devicekey-store.js";

const KEY_EXTENSION = ".bpkey";
const BROWSER_LABEL = "Browser";
const GENERIC_ERROR = "Something went wrong.";
const NO_PASSPHRASE_NOTE = "Without a passphrase, anyone who copies the key file is this device. A passphrase is recommended.";
const ERROR_TEXT = Object.freeze({
  "passphrase-required": "This key file is protected. Enter its passphrase.",
  "authentication-failed": "Wrong passphrase, or the key file was changed.",
  "identity-mismatch": "The key file is damaged: its ID does not match its public or private key.",
  "invalid-id": "The key file's ID is not a valid device ID.",
  "invalid-file": "This file is not a valid device key file.",
  "invalid-input": "That value cannot be used. Check it and try again.",
  "unsupported-format": "This is not a BubblePlay device key file.",
  "unsupported-version": "This key file's version is not supported by this BubblePlay.",
  "unsupported-type": "This key type is not supported. Device keys are Ed25519.",
  "unsupported-kdf": "This key file uses a key derivation this BubblePlay cannot read.",
  "unsupported-aead": "This key file uses an encryption this BubblePlay cannot read.",
});

let sequence = 0;
const nextId = prefix => `${prefix}-${++sequence}`;

/** Default label such as "Chrome on Windows", from the user agent string. */
export function defaultKeyLabel(userAgent = "") {
  const ua = String(userAgent);
  const browser = /Edg\//.test(ua) ? "Edge"
    : /OPR\//.test(ua) ? "Opera"
      : /Firefox\/|FxiOS\//.test(ua) ? "Firefox"
        : /Chrome\/|CriOS\//.test(ua) ? "Chrome"
          : /Safari\//.test(ua) ? "Safari"
            : BROWSER_LABEL;
  // Android before Linux; iOS before macOS, since iPhone user agents also contain "Mac OS X".
  const os = /Android/.test(ua) ? "Android"
    : /iPhone|iPad|iPod/.test(ua) ? "iOS"
      : /Windows/.test(ua) ? "Windows"
        : /CrOS/.test(ua) ? "ChromeOS"
          : /Macintosh|Mac OS X/.test(ua) ? "macOS"
            : /Linux/.test(ua) ? "Linux"
              : "";
  if (!os) return browser;
  return `${browser} on ${os}`;
}

/** Download name for an exported key: the label when it is usable, else the ID. */
export function exportFileName(label, id) {
  const cleaned = Array.from(String(label ?? "").normalize("NFC")
    .replace(/[\u0000-\u001f\u007f<>:"/\\|?*]/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .replace(/^\.+/, "")
    .replace(/[. ]+$/, ""))
    .slice(0, 80)
    .join("")
    .replace(/[. ]+$/, "");
  let base = cleaned || String(id ?? "").trim() || "device-key";
  if (/^(con|prn|aux|nul|com[0-9]|lpt[0-9])$/i.test(base)) base = `${base}-key`;
  return `${base}${KEY_EXTENSION}`;
}

/** Message for a failed device-key step. Codes come from DeviceKeyError in devicekey.js. */
export function deviceKeyErrorMessage(error) {
  if (typeof error?.code === "string" && Object.hasOwn(ERROR_TEXT, error.code)) return ERROR_TEXT[error.code];
  if (typeof error?.message === "string" && error.message) return error.message;
  return GENERIC_ERROR;
}

/** An empty passphrase field means no passphrase. The module refuses "" as a passphrase. */
export function passphraseValue(text) {
  return text === "" ? undefined : text;
}

/** Display fields from a parsed key file. A file with a KDF block is protected by a passphrase. */
export function summarizeKeyFile(file) {
  return {
    id: String(file.id ?? ""),
    label: typeof file.label === "string" ? file.label : "",
    protected: file.kdf !== null && file.kdf !== undefined,
  };
}

function passphraseProblem(values, required) {
  if (required && !values.passphrase) return "Enter a passphrase.";
  if (values.passphrase !== values.confirm) return "The passphrases do not match.";
  return null;
}

function newPassphraseFields() {
  return [
    { name: "passphrase", label: "New passphrase", type: "password", autocomplete: "new-password" },
    { name: "confirm", label: "Confirm passphrase", type: "password", autocomplete: "new-password" },
  ];
}

export async function installDeviceKeyUi({
  doc = globalThis.document,
  store = createDeviceKeyStore(),
  userAgent = globalThis.navigator?.userAgent ?? "",
  clipboard = globalThis.navigator?.clipboard ?? null,
  keys = { createKeyFile, parseKeyFile, rewrapKeyFile },
} = {}) {
  const mount = doc.getElementById("device-key-settings");
  if (!mount) return null;

  const make = (tag, attrs = {}, ...children) => {
    const node = doc.createElement(tag);
    for (const [name, value] of Object.entries(attrs)) {
      if (value === false || value === null || value === undefined) continue;
      if (name === "on") {
        for (const [type, handler] of Object.entries(value)) node.addEventListener(type, handler);
      } else if (value === true) {
        node.setAttribute(name, "");
      } else {
        node.setAttribute(name, String(value));
      }
    }
    node.append(...children.filter(child => child !== null && child !== undefined && child !== false));
    return node;
  };

  // Keys typed into dialogs must not reach the remote PC. The document key handler forwards keys
  // unless focus is inside a local control, so stop these events at the UI root.
  const root = make("div", { id: "devicekey-root", class: "dk-root" });
  root.addEventListener("keydown", event => event.stopPropagation());
  root.addEventListener("keyup", event => event.stopPropagation());
  doc.body.append(root);

  const content = make("div", { class: "dk-settings-content" });
  const statusLine = make("p", { class: "setting-help dk-status", role: "status", "aria-live": "polite" });
  mount.replaceChildren(content, statusLine);

  let entry = null; // { text, info } where info is null when the stored text is unreadable
  let loaded = false;
  let loadError = null;

  function makeEntry(text) {
    try {
      return { text, info: summarizeKeyFile(JSON.parse(text)) };
    } catch {
      return { text, info: null };
    }
  }

  function setStatus(text, tone = "") {
    statusLine.textContent = text;
    statusLine.className = tone ? `setting-help dk-status dk-status-${tone}` : "setting-help dk-status";
  }

  async function act(task) {
    try {
      return await task();
    } catch (error) {
      setStatus(deviceKeyErrorMessage(error), "error");
      return false;
    }
  }

  function saveFile(name, text) {
    const url = URL.createObjectURL(new Blob([text], { type: "application/json" }));
    const link = make("a", { href: url, download: name, hidden: true });
    doc.body.append(link);
    link.click();
    link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  }

  // A modal <dialog>: the browser traps focus, closes on Escape and returns focus to the opener.
  // Busy state disables the controls and blocks Escape until the work finishes.
  function openModal({ title, danger = false, render }) {
    const opener = doc.activeElement;
    const headingId = nextId("dk-title");
    const dialog = make("dialog", { class: danger ? "dk-dialog dk-dialog-danger" : "dk-dialog", "aria-labelledby": headingId, tabindex: -1 });
    let settled = false;
    let busy = false;
    let resolveResult;
    const result = new Promise(resolve => { resolveResult = resolve; });

    const body = make("div", { class: "dk-dialog-body" });
    const content = make("div", { class: "dk-content" });
    const error = make("p", { class: "dk-error", role: "alert" });
    const busyLine = make("p", { class: "dk-busy", role: "status", "aria-live": "polite" });
    const closeButton = make("button", { type: "button", class: "panel-close", "aria-label": "Close", on: { click: () => finish(null) } }, "×");
    body.append(content, error, busyLine);
    dialog.append(make("div", { class: "dk-dialog-head" }, make("h2", { id: headingId }, title), closeButton), body);

    function finish(value) {
      if (settled) return;
      settled = true;
      resolveResult(value);
      dialog.close();
    }

    // The settings panel listens for Escape in the document capture phase and would close itself
    // instead of this dialog. Window capture runs first, so the open dialog takes the key here.
    const view = doc.defaultView;
    const onEscape = event => {
      if (event.key !== "Escape" || !dialog.open) return;
      event.preventDefault();
      event.stopImmediatePropagation();
      if (!busy) finish(null);
    };
    view?.addEventListener("keydown", onEscape, true);

    const modal = {
      body: content,
      finish,
      setError(text) { error.textContent = text || ""; },
      setBusy(on, text = "") {
        busy = on;
        busyLine.textContent = on ? text : "";
        dialog.setAttribute("aria-busy", on ? "true" : "false");
        for (const control of dialog.querySelectorAll("button, input, select")) control.disabled = on;
        if (on) dialog.focus({ preventScroll: true });
      },
      focusInitial() {
        (content.querySelector("[data-initial-focus]") ?? content.querySelector("input, select, button[type=submit]"))?.focus({ preventScroll: true });
      },
    };

    dialog.addEventListener("cancel", event => { if (busy) event.preventDefault(); });
    dialog.addEventListener("close", () => {
      view?.removeEventListener("keydown", onEscape, true);
      dialog.remove();
      if (!settled) {
        settled = true;
        resolveResult(null);
      }
      if (opener?.isConnected) opener.focus({ preventScroll: true });
    });

    root.append(dialog);
    render(modal);
    dialog.showModal();
    modal.focusInitial();
    return result;
  }

  // One form dialog: fields, an optional note, a submit label that may depend on the values,
  // a synchronous check, and the async work whose result closes the dialog. Resolves to that
  // result, or null on cancel.
  function formModal({ title, lead, fields = [], note, submitLabel, danger = false, busyText = "Working…", validate, run }) {
    return openModal({
      title,
      danger,
      render(modal) {
        const controls = new Map();
        const form = make("form", { class: "dk-form", novalidate: true });
        if (lead) form.append(make("p", { class: "dk-lead" }, lead));

        for (const field of fields) {
          if (field.type === "radio") {
            const group = make("fieldset", { class: "dk-radio" }, make("legend", { class: "field-label" }, field.label));
            for (const option of field.options) {
              group.append(make("label", { class: "dk-option" },
                make("input", { type: "radio", name: field.name, value: option.value, checked: option.value === field.value }),
                make("span", { class: "dk-option-title" }, option.label),
                make("span", { class: "dk-option-help" }, option.help)));
            }
            form.append(group);
            controls.set(field.name, { field, group });
            continue;
          }
          const id = nextId("dk-field");
          const input = make("input", {
            id,
            type: field.type ?? "text",
            name: field.name,
            autocomplete: field.autocomplete ?? "off",
            maxlength: field.maxLength,
            accept: field.accept,
            spellcheck: field.type === "password" ? "false" : undefined,
            autocapitalize: field.type === "password" ? "none" : undefined,
          });
          if (field.value !== undefined && field.type !== "file") input.value = field.value;
          const label = make("label", { class: "field-label", for: id }, field.label);
          const wrap = make("div", { class: "dk-field" }, label, input);
          if (field.help) wrap.append(make("p", { class: "dk-help" }, field.help));
          form.append(wrap);
          controls.set(field.name, { field, input });
        }

        if (note) form.append(make("p", { class: "dk-note" }, note));
        const submit = make("button", { type: "submit", class: danger ? "dk-danger-action" : "dk-primary" });
        const cancel = make("button", { type: "button", class: "dk-secondary", "data-initial-focus": danger, on: { click: () => modal.finish(null) } }, "Cancel");
        form.append(make("div", { class: "dk-actions" }, submit, cancel));
        modal.body.append(form);

        const readValues = () => Object.fromEntries([...controls].map(([name, { field, input, group }]) => {
          if (field.type === "radio") return [name, group.querySelector("input:checked")?.value ?? field.value];
          if (field.type === "file") return [name, input.files?.[0] ?? null];
          return [name, input.value];
        }));
        const refreshSubmit = () => {
          submit.textContent = typeof submitLabel === "function" ? submitLabel(readValues()) : submitLabel;
        };
        form.addEventListener("input", refreshSubmit);
        form.addEventListener("change", refreshSubmit);
        refreshSubmit();

        form.addEventListener("submit", async event => {
          event.preventDefault();
          const values = readValues();
          const problem = validate?.(values);
          if (problem) {
            modal.setError(problem);
            return;
          }
          modal.setError("");
          modal.setBusy(true, busyText);
          try {
            modal.finish(await run(values));
          } catch (failure) {
            modal.setBusy(false);
            if (failure?.code === "authentication-failed") {
              for (const { field, input } of controls.values()) {
                if (field.type === "password" && input) input.value = "";
              }
            }
            modal.setError(deviceKeyErrorMessage(failure));
            modal.focusInitial();
          }
        });
      },
    });
  }

  // The reusable passphrase prompt: unlocks a stored or imported key for one step.
  function passphrasePrompt({ title, lead, submitLabel = "Continue", run }) {
    return formModal({
      title,
      lead,
      fields: [{ name: "passphrase", label: "Passphrase", type: "password", autocomplete: "current-password" }],
      submitLabel,
      busyText: "Checking passphrase… this takes about a second.",
      validate: values => (values.passphrase ? null : "Enter the passphrase."),
      run: values => run(values.passphrase),
    });
  }

  function confirmFlow({ title, lead, submitLabel, danger = false }) {
    return formModal({ title, lead, submitLabel, danger, run: () => true });
  }

  async function createFlow() {
    const file = await formModal({
      title: "Create device key",
      lead: "This browser gets its own key. You can export a copy right after creating it.",
      fields: [
        { name: "label", label: "Label", value: defaultKeyLabel(userAgent), maxLength: 64 },
        { name: "passphrase", label: "Passphrase (recommended)", type: "password", autocomplete: "new-password" },
        { name: "confirm", label: "Confirm passphrase", type: "password", autocomplete: "new-password" },
      ],
      note: NO_PASSPHRASE_NOTE,
      submitLabel: values => (values.passphrase ? "Create key" : "Create without passphrase"),
      busyText: "Creating key… a passphrase takes about a second.",
      validate: values => passphraseProblem(values, false),
      run: values => keys.createKeyFile({
        label: values.label.trim() || defaultKeyLabel(userAgent),
        passphrase: passphraseValue(values.passphrase),
      }),
    });
    if (!file) return false;
    const text = JSON.stringify(file, null, 2);
    await store.save(text);
    entry = makeEntry(text);
    renderSettings();
    setStatus(`Key created: ${entry.info?.label || entry.info?.id}.`);
    await act(() => exportFlow("Key created. Export a copy now, and keep it somewhere safe."));
    return true;
  }

  async function importFlow() {
    const picked = await formModal({
      title: "Import device key",
      lead: "Choose a .bpkey file. Its ID and passphrase are checked before anything in this browser changes.",
      fields: [{ name: "file", label: "Key file (.bpkey)", type: "file", accept: `${KEY_EXTENSION},application/json` }],
      submitLabel: "Import",
      busyText: "Checking key file…",
      validate: values => (values.file ? null : "Choose a .bpkey file."),
      run: async values => {
        const text = await values.file.text();
        try {
          const file = await keys.parseKeyFile(text);
          return { text, file, protected: false };
        } catch (error) {
          if (error?.code === "passphrase-required") return { text, file: null, protected: true };
          throw error;
        }
      },
    });
    if (!picked) return false;

    let file = picked.file;
    let stored = picked.text;
    if (picked.protected) {
      const checked = await passphrasePrompt({
        title: "Import device key",
        lead: "This key file is protected. Enter its passphrase to check it.",
        submitLabel: "Check key",
        run: async passphrase => ({
          file: await keys.parseKeyFile(picked.text, passphrase),
          text: picked.text,
        }),
      });
      if (!checked) return false;
      file = checked.file;
      stored = checked.text;
    }

    if (entry) {
      const replace = await confirmFlow({
        title: "Replace this browser's key?",
        lead: `Its current ID, ${entry.info?.id ?? "unreadable"}, is removed. Without an exported copy it cannot be recovered.`,
        submitLabel: "Replace key",
        danger: true,
      });
      if (!replace) return false;
    }

    // The file is stored byte for byte, as it was checked.
    await store.save(stored);
    entry = makeEntry(stored);
    renderSettings();
    setStatus(`Key imported: ${summarizeKeyFile(file).label || file.id}.`);
    return true;
  }

  // Export: keep the stored file as it is, or make a copy with a new passphrase or none (rewrap).
  // The stored key itself never changes here.
  async function exportFlow(lead) {
    const { text, info } = entry;
    const name = exportFileName(info.label, info.id);
    const options = info.protected
      ? [
        { value: "keep", label: "Keep the current passphrase", help: "Downloads the file exactly as stored." },
        { value: "new", label: "Use a new passphrase", help: "Downloads a copy under a new passphrase. This browser keeps its key." },
        { value: "none", label: "No passphrase", help: "The copy is not protected. Anyone who has it is this device." },
      ]
      : [
        { value: "keep", label: "No passphrase, as stored", help: "Downloads the file exactly as stored." },
        { value: "new", label: "Add a passphrase", help: "Downloads a copy protected by a passphrase. This browser keeps its key." },
      ];
    const mode = await formModal({
      title: "Export device key",
      lead: lead ?? `Downloads ${name}.`,
      fields: [{ name: "mode", type: "radio", value: "keep", options }],
      submitLabel: "Export",
      run: values => values.mode,
    });
    if (!mode) return false;
    if (mode === "keep") {
      saveFile(name, text);
      setStatus(`Exported ${name}.`);
      return true;
    }

    let copy;
    if (mode === "new" && !info.protected) {
      copy = await formModal({
        title: "Export device key",
        lead: "Choose the passphrase for the exported copy. This browser keeps its key unchanged.",
        fields: newPassphraseFields(),
        note: NO_PASSPHRASE_NOTE,
        submitLabel: "Export",
        busyText: "Encrypting the copy… this takes about a second.",
        validate: values => passphraseProblem(values, true),
        run: values => keys.rewrapKeyFile(text, undefined, values.passphrase),
      });
    } else {
      let next;
      if (mode === "new") {
        next = await formModal({
          title: "Export device key",
          lead: "Choose the passphrase for the exported copy. This browser keeps its key unchanged.",
          fields: newPassphraseFields(),
          note: NO_PASSPHRASE_NOTE,
          submitLabel: "Continue",
          validate: values => passphraseProblem(values, true),
          run: values => values.passphrase,
        });
        if (!next) return false;
      }
      copy = await passphrasePrompt({
        title: "Export device key",
        lead: "Enter this key's current passphrase to make the copy.",
        submitLabel: "Export",
        run: passphrase => keys.rewrapKeyFile(text, passphrase, next),
      });
    }
    if (!copy) return false;
    saveFile(name, JSON.stringify(copy, null, 2));
    setStatus(`Exported ${name}.`);
    return true;
  }

  async function changeFlow() {
    const { text, info } = entry;
    let current;
    if (info.protected) {
      current = await passphrasePrompt({
        title: "Change passphrase",
        lead: "Enter the current passphrase first.",
        run: passphrase => keys.parseKeyFile(text, passphrase).then(() => passphrase),
      });
      if (!current) return false;
    }
    const result = await formModal({
      title: "Change passphrase",
      lead: info.protected ? "Choose the new passphrase. Leave both fields empty to remove it." : "Add a passphrase to this key.",
      fields: newPassphraseFields(),
      note: NO_PASSPHRASE_NOTE,
      submitLabel: values => (values.passphrase ? "Save passphrase" : "Save without passphrase"),
      busyText: "Encrypting… this takes about a second.",
      validate: values => passphraseProblem(values, false),
      run: async values => ({ file: await keys.rewrapKeyFile(text, current, passphraseValue(values.passphrase)) }),
    });
    if (!result) return false;
    const next = JSON.stringify(result.file, null, 2);
    await store.save(next);
    entry = makeEntry(next);
    renderSettings();
    setStatus("Passphrase updated.");
    return true;
  }

  async function removeFlow() {
    const confirmed = await confirmFlow({
      title: "Remove this browser's key?",
      lead: "Without an exported copy it cannot be recovered.",
      submitLabel: "Remove key",
      danger: true,
    });
    if (!confirmed) return false;
    await store.clear();
    entry = null;
    renderSettings();
    setStatus("Key removed from this browser.");
    return true;
  }

  async function copyId() {
    try {
      await clipboard.writeText(entry.info.id);
      setStatus("Key ID copied.");
    } catch {
      setStatus("Could not copy the ID. Select it and copy it instead.", "error");
    }
  }

  const actionButton = (label, handler, className = "") => make("button", {
    type: "button",
    class: className || undefined,
    on: { click: () => act(handler) },
  }, label);

  function renderSettings() {
    content.replaceChildren();
    const help = text => make("p", { class: "setting-help" }, text);
    if (loadError) {
      content.append(help(`Device keys are unavailable in this browser. ${deviceKeyErrorMessage(loadError)}`));
      return;
    }
    if (!loaded) {
      content.append(help("Checking this browser's key…"));
      return;
    }
    if (!entry) {
      content.append(
        help("This browser has no device key."),
        make("div", { class: "dk-actions" },
          actionButton("Create key", createFlow, "dk-primary"),
          actionButton("Import key", importFlow)));
      return;
    }
    if (!entry.info) {
      content.append(
        help("The stored key could not be read. Remove it, then create or import a key."),
        make("div", { class: "dk-actions" },
          actionButton("Remove key", removeFlow, "dk-danger-action"),
          actionButton("Import key", importFlow)));
      return;
    }
    const { info } = entry;
    content.append(
      make("dl", { class: "dk-facts" },
        make("dt", {}, "Key ID"),
        make("dd", {}, make("code", { class: "dk-id" }, info.id), " ", make("button", { type: "button", class: "dk-copy", on: { click: copyId } }, "Copy ID")),
        make("dt", {}, "Label"),
        make("dd", {}, info.label || "No label"),
        make("dt", {}, "Passphrase"),
        make("dd", {}, info.protected ? "Protected by a passphrase" : "None")),
      make("div", { class: "dk-actions" },
        actionButton("Export key", () => exportFlow()),
        actionButton("Change passphrase", changeFlow),
        actionButton("Remove key", removeFlow, "dk-danger-action")));
  }

  renderSettings();
  try {
    const text = await store.load();
    // A stored key in another format counts as no key: create or import replaces it.
    entry = text === null || text === undefined || isOtherFormatKeyFileText(text) ? null : makeEntry(text);
  } catch (error) {
    loadError = error;
  }
  loaded = true;
  renderSettings();
  // For the device-key gate: each resolves true once a key is stored, false when cancelled.
  // Errors propagate to the caller, which shows them.
  return { createKey: () => createFlow(), importKey: () => importFlow() };
}
