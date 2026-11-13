// Text entry through the on-screen keyboard field. The field holds a draft; edits are turned into
// backspaces and text packets by diffing against what was already sent (the shadow). On a secure
// desktop only protected input is sent: no corrections, paste or autocomplete.
import { reconcileTextEdit, secureTextUnits } from "./input.js";
import { createCompositionCommitGate } from "./composition-gate.js";

export const BACKSPACE_VK = 0x08;
// Under the 512-byte text packet limit of input.js and the host's input.rs.
export const TEXT_CHUNK_BYTES = 500;
const DESKTOP_INPUT_ENABLED = "enabled";
const SECURE_DESKTOP = "secure";
const INSERT_TEXT = "insertText";
const INSERT_COMPOSITION_TEXT = "insertCompositionText";
const DELETE_BACKWARD = "deleteContentBackward";
const PROTECTED_SENT = "Protected input sent.";
const PROTECTED_BLOCKED = "Paste and automatic corrections are disabled on secure desktop.";

function plural(count, word) { return `${count} ${word}${count === 1 ? "" : "s"}`; }

// env: input (InputController), field (the text input element), inputReady(), desktopKind(),
// desktopInputState(), setStatus(text), schedule/cancel (optional, for tests).
export function createKeyboardDraft(env) {
  const encoder = new TextEncoder();
  let shadow = "";
  let composing = false;

  const gate = createCompositionCommitGate(text => {
    if (!env.inputReady() || env.desktopKind() !== SECURE_DESKTOP) { reset(); return; }
    sendProtected({ data: text, inputType: INSERT_COMPOSITION_TEXT });
  }, env.schedule, env.cancel);

  function reset() {
    composing = false;
    gate.reset();
    shadow = "";
    env.field.value = "";
  }

  function sendText(text) {
    let chunk = "";
    let sent = 0;
    for (const character of String(text)) {
      const candidate = chunk + character;
      if (encoder.encode(candidate).byteLength > TEXT_CHUNK_BYTES && chunk) {
        if (env.input.text(chunk)) sent += Array.from(chunk).length;
        chunk = character;
      } else chunk = candidate;
    }
    if (chunk && env.input.text(chunk)) sent += Array.from(chunk).length;
    return sent;
  }

  function backspace() { env.input.key(BACKSPACE_VK, false); env.input.key(BACKSPACE_VK, true); }

  function reconcile(nextValue) {
    const next = String(nextValue ?? "");
    const edit = reconcileTextEdit(shadow, next);
    if (env.inputReady()) {
      for (let i = 0; i < edit.deleteCount; i++) backspace();
      const inserted = sendText(edit.insert);
      if (edit.deleteCount || inserted) {
        env.setStatus(edit.deleteCount
          ? `Applied correction: ${plural(edit.deleteCount, "delete")}${inserted ? ` + ${plural(inserted, "character")}` : ""}.`
          : `Sent ${plural(inserted, "character")}.`);
      }
    }
    shadow = next;
  }

  function sendProtected(event) {
    if (env.desktopInputState() !== DESKTOP_INPUT_ENABLED) { reset(); return; }
    if (event.inputType === DELETE_BACKWARD) {
      backspace();
      env.setStatus(PROTECTED_SENT);
      reset();
      return;
    }
    const units = secureTextUnits(event.data ?? env.field.value, event.inputType || INSERT_TEXT);
    for (const unit of units) env.input.text(unit);
    env.setStatus(units.length ? PROTECTED_SENT : PROTECTED_BLOCKED);
    reset();
  }

  return {
    reset,
    compositionStart() { gate.reset(); composing = true; },
    compositionEnd(event) {
      composing = false;
      if (env.desktopKind() === SECURE_DESKTOP) {
        if (env.inputReady()) gate.stage(event.data ?? env.field.value);
        else reset();
        return;
      }
      if (env.inputReady()) reconcile(env.field.value);
    },
    input(event) {
      if (!env.inputReady() || event.isComposing || composing) return;
      if (env.desktopKind() === SECURE_DESKTOP) {
        if (gate.consume(event.data ?? env.field.value)) return;
        sendProtected(event);
        return;
      }
      reconcile(env.field.value);
    },
  };
}
