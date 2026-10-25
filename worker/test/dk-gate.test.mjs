import test from "node:test";
import assert from "node:assert/strict";
import { mountGate } from "../public/dk-gate.js";

// A minimal document: enough for dk-gate.js, which must not touch innerHTML.
class FakeNode {
  constructor(doc, tagName) {
    this.ownerDocument = doc;
    this.tagName = tagName.toUpperCase();
    this.parentNode = null;
    this.children = [];
    this.listeners = {};
    this.attributes = {};
    this.className = "";
    this.id = "";
    this.type = "";
    this.hidden = false;
    this.disabled = false;
    this.value = "";
    this.htmlFor = "";
    this.text = "";
  }

  get textContent() {
    return this.text + this.children.map(child => child.textContent).join("");
  }

  set textContent(value) {
    this.text = String(value);
    this.children = [];
  }

  set innerHTML(_value) {
    throw new Error("dk-gate.js must not use innerHTML");
  }

  appendChild(node) {
    node.remove();
    node.parentNode = this;
    this.children.push(node);
    return node;
  }

  append(...nodes) {
    for (const node of nodes) this.appendChild(node);
  }

  replaceChildren(...nodes) {
    for (const child of this.children) child.parentNode = null;
    this.children = [];
    this.text = "";
    this.append(...nodes);
  }

  remove() {
    if (!this.parentNode) return;
    this.parentNode.children = this.parentNode.children.filter(child => child !== this);
    this.parentNode = null;
  }

  setAttribute(name, value) {
    this.attributes[name] = String(value);
  }

  addEventListener(type, listener) {
    (this.listeners[type] ??= []).push(listener);
  }

  focus() {
    this.ownerDocument.activeElement = this;
  }

  dispatch(type, extra = {}) {
    const event = { type, target: this, preventDefault() {}, ...extra };
    for (const listener of this.listeners[type] ?? []) listener(event);
  }

  // Like a browser: a submit button submits its form and a disabled control does nothing.
  click() {
    if (this.disabled) return;
    if (this.type === "submit") {
      let form = this.parentNode;
      while (form && form.tagName !== "FORM") form = form.parentNode;
      form?.dispatch("submit");
      return;
    }
    this.dispatch("click");
  }
}

function fakeDocument() {
  const doc = {
    activeElement: null,
    createElement: tagName => new FakeNode(doc, tagName),
    getElementById: id => nodes(doc.body).find(node => node.id === id) ?? null,
  };
  doc.body = new FakeNode(doc, "body");
  return doc;
}

const flush = () => new Promise(resolve => setImmediate(resolve));

function nodes(root) {
  const out = [];
  const visit = node => {
    out.push(node);
    node.children.forEach(visit);
  };
  visit(root);
  return out;
}

function one(root, predicate, what) {
  const [hit] = nodes(root).filter(predicate);
  assert.ok(hit, `missing ${what}`);
  return hit;
}

const title = root => one(root, node => node.className === "dk-gate-title", "title").textContent;
const button = (root, label) => one(root, node => node.tagName === "BUTTON" && node.textContent === label, `button "${label}"`);
const field = (root, label) => one(root, node => node.tagName === "LABEL" && node.textContent === label, `label "${label}"`)
  .parentNode.children.find(node => node.tagName === "INPUT");
const alertText = root => nodes(root).find(node => node.attributes.role === "alert" && node.textContent)?.textContent ?? "";
const statusText = root => nodes(root).find(node => node.attributes.role === "status")?.textContent ?? "";
const hasText = (root, text) => nodes(root).some(node => node.textContent === text);

test("unlock sends the typed passphrase, labels the dialog and clears busy after success", async () => {
  const doc = fakeDocument();
  const gate = mountGate(doc);
  const root = doc.getElementById("dk-gate");
  const sent = [];
  gate.showKeyScreen({ mode: "unlock", onUnlock: async passphrase => { sent.push(passphrase); }, onRemove: async () => {} });
  assert.equal(root.hidden, false);
  assert.equal(root.attributes["aria-modal"], "true");
  assert.equal(root.attributes["aria-labelledby"], one(root, node => node.className === "dk-gate-title").id);
  assert.equal(title(root), "Unlock this browser");
  assert.equal(hasText(root, "Your device key is protected with a passphrase."), true);
  const passphrase = field(root, "Passphrase");
  assert.equal(doc.activeElement, passphrase, "the passphrase field takes focus");
  passphrase.value = "correct horse";
  button(root, "Unlock").click();
  await flush();
  assert.deepEqual(sent, ["correct horse"]);
  assert.equal(button(root, "Unlock").disabled, false);
  assert.equal(statusText(root), "");
});

test("a wrong passphrase shows the error, keeps the typed value and refocuses the field", async () => {
  const doc = fakeDocument();
  const gate = mountGate(doc);
  const root = doc.getElementById("dk-gate");
  gate.showKeyScreen({
    mode: "unlock",
    onUnlock: async () => {
      throw Object.assign(new Error("That passphrase did not unlock the key."), { code: "wrong-passphrase" });
    },
  });
  const passphrase = field(root, "Passphrase");
  passphrase.value = "oops";
  doc.activeElement = null;
  button(root, "Unlock").click();
  await flush();
  assert.equal(alertText(root), "That passphrase did not unlock the key.");
  assert.equal(passphrase.value, "oops");
  assert.equal(doc.activeElement, passphrase);
  assert.equal(button(root, "Unlock").disabled, false);
});

test("while an unlock is pending its buttons are disabled, the status says so and a second submit is ignored", async () => {
  const doc = fakeDocument();
  const gate = mountGate(doc);
  const root = doc.getElementById("dk-gate");
  const calls = [];
  let finish;
  gate.showKeyScreen({ mode: "unlock", onUnlock: passphrase => { calls.push(passphrase); return new Promise(resolve => { finish = resolve; }); } });
  field(root, "Passphrase").value = "slow";
  button(root, "Unlock").click();
  assert.equal(button(root, "Unlock").disabled, true);
  assert.equal(statusText(root), "Unlocking key…");
  button(root, "Unlock").click();
  await flush();
  assert.deepEqual(calls, ["slow"]);
  finish();
  await flush();
  assert.equal(button(root, "Unlock").disabled, false);
  assert.equal(statusText(root), "");
});

test("setup offers Create key as the primary action and Import key file, and each calls its callback", async () => {
  const doc = fakeDocument();
  const gate = mountGate(doc);
  const root = doc.getElementById("dk-gate");
  let created = 0;
  let imported = 0;
  gate.showKeyScreen({ mode: "setup", onCreate: async () => { created++; }, onImport: async () => { imported++; } });
  assert.equal(title(root), "Set up this browser");
  assert.equal(button(root, "Create key").className.includes("dk-gate-primary"), true);
  assert.equal(button(root, "Import key file").className.includes("dk-gate-primary"), false);
  assert.equal(hasText(root, "Use a different key"), false, "no key is stored, so nothing to remove");
  button(root, "Create key").click();
  await flush();
  button(root, "Import key file").click();
  await flush();
  assert.equal(created, 1);
  assert.equal(imported, 1);
});

test("the PC list connects, renames, removes after a confirm, and passes pairing strings raw", async () => {
  const doc = fakeDocument();
  const gate = mountGate(doc);
  const root = doc.getElementById("dk-gate");
  const pcs = [
    { id: "ABCD-1111-2222-3333", name: "Office", lastUsed: null },
    { id: "EFGH-4444-5555-6666", name: "Den", lastUsed: Date.now() - 3 * 3600e3 },
  ];
  const log = { connect: [], rename: [], remove: [], pair: [] };
  let pairError = null;
  gate.showPcList({
    deviceId: "DEV-0000-1111-2222",
    pcs,
    onConnect: id => log.connect.push(id),
    onRename: async (id, name) => { log.rename.push([id, name]); },
    onRemove: async id => { log.remove.push(id); },
    onPair: async args => {
      log.pair.push(args);
      if (pairError) throw pairError;
    },
    onForgetBrowser: async () => {},
    onLock: () => {},
  });
  assert.equal(title(root), "Your PCs");
  assert.equal(hasText(root, "This browser: DEV-0000-1111-2222"), true);
  assert.equal(hasText(root, "Last used 3 h ago"), true);
  assert.equal(hasText(root, "Not used yet"), true);

  const row = nodes(root).find(node => node.className === "dk-gate-pc" && node.textContent.includes("Office"));
  one(row, node => node.className === "dk-gate-pc-main", "row button").click();
  assert.deepEqual(log.connect, ["ABCD-1111-2222-3333"]);

  button(row, "Rename").click();
  field(row, "Name for this PC").value = "Office HQ";
  button(row, "Save name").click();
  await flush();
  assert.deepEqual(log.rename, [["ABCD-1111-2222-3333", "Office HQ"]]);
  assert.equal(hasText(row, "Save name"), false, "the rename field closes after it saves");

  button(row, "Remove").click();
  assert.equal(
    hasText(row, "Remove PC 'Office' (ABCD-1111-2222-3333)? This browser forgets its key; the PC keeps its own list."),
    true,
  );
  button(row, "Keep").click();
  await flush();
  assert.deepEqual(log.remove, []);
  assert.equal(hasText(row, "Keep"), false);
  button(row, "Remove").click();
  button(row, "Confirm").click();
  await flush();
  assert.deepEqual(log.remove, ["ABCD-1111-2222-3333"]);

  assert.equal(button(root, "Pair").className.includes("dk-gate-secondary"), true, "Pair is secondary while PCs are listed");
  field(root, "PC ID").value = " abcd 1111 ";
  field(root, "Pairing code").value = "12-34";
  button(root, "Pair").click();
  await flush();
  assert.deepEqual(log.pair, [{ pc: " abcd 1111 ", code: "12-34" }]);

  pairError = new Error("That code is not valid. Check it on the PC.");
  button(root, "Pair").click();
  await flush();
  assert.equal(alertText(root), "That code is not valid. Check it on the PC.");
  assert.equal(field(root, "PC ID").value, " abcd 1111 ");
  assert.equal(field(root, "Pairing code").value, "12-34");
});

test("an empty PC list says so and makes Pair the primary action", () => {
  const doc = fakeDocument();
  const gate = mountGate(doc);
  const root = doc.getElementById("dk-gate");
  gate.showPcList({ deviceId: "DEV-0000-1111-2222", pcs: [], onPair: async () => {}, onLock() {}, onForgetBrowser: async () => {} });
  assert.equal(hasText(root, "No PCs paired with this browser yet."), true);
  assert.equal(button(root, "Pair").className.includes("dk-gate-primary"), true);
});

test("the comparison number is grouped for sight and read digit by digit, and Cancel fires", async () => {
  const doc = fakeDocument();
  const gate = mountGate(doc);
  const root = doc.getElementById("dk-gate");
  let cancelled = 0;
  gate.showPairing({ state: "compare", pcId: "ABCD-1111-2222-3333", number: "482913", onCancel: () => { cancelled++; } });
  assert.equal(title(root), "Compare the number");
  assert.equal(hasText(root, "ABCD-1111-2222-3333"), true, "the PC ID sits under the title");
  const digits = one(root, node => node.className === "dk-gate-number", "comparison number");
  assert.equal(digits.textContent, "482 913");
  assert.equal(digits.attributes["aria-label"], "4 8 2 9 1 3");
  button(root, "Cancel").click();
  await flush();
  assert.equal(cancelled, 1);
});

test("done offers the PC ID as the default name and saves the edited name", async () => {
  const doc = fakeDocument();
  const gate = mountGate(doc);
  const root = doc.getElementById("dk-gate");
  const saved = [];
  gate.showPairing({ state: "done", pcId: "ABCD-1111-2222-3333", onSave: async name => { saved.push(name); }, onBack() {} });
  assert.equal(title(root), "Paired");
  const name = field(root, "Name for this PC");
  assert.equal(name.value, "ABCD-1111-2222-3333");
  name.value = "Office";
  button(root, "Save and connect").click();
  await flush();
  assert.deepEqual(saved, ["Office"]);
});

test("denied and failed show default copy or the logic's message, and Back to PCs fires", async () => {
  const doc = fakeDocument();
  const gate = mountGate(doc);
  const root = doc.getElementById("dk-gate");
  let backs = 0;
  gate.showPairing({ state: "denied", pcId: "ABCD-1111-2222-3333", onBack: () => { backs++; } });
  assert.equal(title(root), "Pairing declined");
  assert.equal(hasText(root, "The PC declined. Nothing was saved."), true);
  button(root, "Back to PCs").click();
  await flush();
  assert.equal(backs, 1);

  gate.showPairing({ state: "failed", pcId: "ABCD-1111-2222-3333", message: "The relay closed the pairing.", onBack: () => { backs++; } });
  assert.equal(title(root), "Pairing failed");
  assert.equal(hasText(root, "The relay closed the pairing."), true);
  assert.equal(hasText(root, "Nothing was saved."), false);
});

test("a refusal shows its recovery copy and its action fires", async () => {
  const doc = fakeDocument();
  const gate = mountGate(doc);
  const root = doc.getElementById("dk-gate");
  let acted = 0;
  let other = 0;
  gate.showRefusal({
    title: "PC key changed",
    body: "This PC answered with a different key than the one this browser paired with.",
    actionLabel: "Pair again",
    onAction: () => { acted++; },
    secondaryLabel: "Back to PCs",
    onSecondary: () => { other++; },
  });
  assert.equal(title(root), "PC key changed");
  one(root, node => node.className.includes("dk-gate-refusal"), "refusal panel");
  button(root, "Pair again").click();
  await flush();
  assert.equal(acted, 1);
  assert.equal(other, 0);
});
