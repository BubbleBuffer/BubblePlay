import test from "node:test";
import assert from "node:assert/strict";
import { FileTransferClient } from "../public/file-client.js";
import { installFileTransferUi } from "../public/file-ui.js";

function element() {
  return {
    hidden: true, textContent: "", style: {}, dataset: {}, attributes: {}, children: [],
    classList: { add() {} }, addEventListener() {},
    append(...children) { this.children.push(...children); },
    replaceChildren(...children) { this.children = children; },
    setAttribute(name, value) { this.attributes[name] = value; },
  };
}

test("a picker-open offer replacement or withdrawal shows an error and downloads nothing", async () => {
  const keys = ["window", "document", "showSaveFilePicker", "showDirectoryPicker"];
  const saved = keys.map(key => globalThis[key]);
  globalThis.window = { addEventListener() {} };
  globalThis.document = { body: element(), createElement: element, addEventListener() {} };
  globalThis.showDirectoryPicker = async () => { throw new Error("single-file offer uses the save picker"); };
  try {
    for (const replacementItems of [[{ id: 1, name: "replacement.txt", size: 2 }], []]) {
      let finishPicker, writableCalls = 0, socketCalls = 0;
      const pickerCalls = [];
      globalThis.showSaveFilePicker = options => {
        pickerCalls.push(options);
        return new Promise(resolve => { finishPicker = resolve; });
      };
      const client = new FileTransferClient({ socketUrl: () => "wss://x", WebSocketImpl: class { constructor() { socketCalls++; } } });
      const offer = { type: "file-offer", protocol: 1, offerId: 1, source: "clipboard", items: [{ id: 1, name: "original.txt", size: 1 }] };
      client.setOffer(offer);
      const parts = new Map();
      const root = { hidden: true, querySelector(selector) { if (!parts.has(selector)) parts.set(selector, element()); return parts.get(selector); } };
      const veil = { ...element(), querySelector: () => element() };
      const errors = [];
      const ui = installFileTransferUi({ client, root, veil, onError: error => errors.push(error) });
      ui.setEnabled(true);
      const click = parts.get("[data-file-offer-download]").onclick({ stopPropagation() {} });
      assert.deepEqual(pickerCalls, [{ suggestedName: "original.txt" }]);
      client.setOffer({ ...offer, offerId: 2, items: replacementItems });
      finishPicker({ createWritable() { writableCalls++; throw new Error("stale offer must not open a sink"); } });
      await click;
      assert.equal(errors.length, 1);
      assert.equal(errors[0].message, "The PC replaced these files. Try again.");
      assert.equal(client.transfers.length, 0);
      assert.equal(writableCalls, 0);
      assert.equal(socketCalls, 0);
      assert.equal(root.hidden, false, "the error stays visible even when the offer was withdrawn");
      const [errorRow] = parts.get("[data-file-list]").children;
      const [status, dismiss] = errorRow.children;
      assert.equal(status.textContent, "The PC replaced these files. Try again.");
      assert.equal(status.attributes.role, "status");
      assert.equal(parts.get("[data-file-offer]").hidden, !replacementItems.length, "a replacement remains available for another click");
      dismiss.onclick({ stopPropagation() {} });
      assert.equal(parts.get("[data-file-list]").children.length, 0);
      assert.equal(root.hidden, !replacementItems.length);
      ui.setEnabled(false);
      client.reset();
    }
  } finally {
    keys.forEach((key, index) => { if (saved[index] === undefined) delete globalThis[key]; else globalThis[key] = saved[index]; });
  }
});

test("an unchanged picker offer downloads its captured id and hides its chip", async () => {
  const keys = ["window", "document", "showSaveFilePicker", "showDirectoryPicker"];
  const saved = keys.map(key => globalThis[key]);
  globalThis.window = { addEventListener() {} };
  globalThis.document = { body: element(), createElement: element, addEventListener() {} };
  globalThis.showDirectoryPicker = async () => {};
  globalThis.showSaveFilePicker = async () => ({ createWritable() { throw new Error("only sink selection is tested"); } });
  try {
    const client = new EventTarget();
    client.offer = { offerId: 7, items: [{ id: 3, name: "a.txt", size: 1 }] };
    client.transfers = [];
    const downloads = [];
    client.download = (...args) => { downloads.push(args); client.dispatchEvent(new Event("change")); };
    const parts = new Map();
    const root = { hidden: true, querySelector(selector) { if (!parts.has(selector)) parts.set(selector, element()); return parts.get(selector); } };
    const veil = { ...element(), querySelector: () => element() };
    const ui = installFileTransferUi({ client, root, veil, onError: error => { throw error; } });
    ui.setEnabled(true);
    await parts.get("[data-file-offer-download]").onclick({ stopPropagation() {} });
    assert.equal(downloads.length, 1);
    assert.deepEqual(downloads[0][0], client.offer.items);
    assert.equal(typeof downloads[0][1], "function");
    assert.equal(downloads[0][2], 7);
    assert.equal(parts.get("[data-file-offer]").hidden, true);
    ui.setEnabled(false);
  } finally {
    keys.forEach((key, index) => { if (saved[index] === undefined) delete globalThis[key]; else globalThis[key] = saved[index]; });
  }
});
