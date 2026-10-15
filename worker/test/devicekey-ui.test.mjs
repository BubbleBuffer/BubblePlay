import { test } from "node:test";
import assert from "node:assert/strict";
import {
  OFFER_DISMISSED_KEY, defaultKeyLabel, deviceKeyErrorMessage, exportFileName, passphraseValue,
  readOfferDismissed, shouldOfferKey, summarizeKeyFile, writeOfferDismissed,
} from "../public/devicekey-ui.js";

function memoryStorage() {
  const items = new Map();
  return {
    items,
    getItem: key => (items.has(key) ? items.get(key) : null),
    setItem: (key, value) => { items.set(key, String(value)); },
  };
}

const throwingStorage = {
  getItem() { throw new DOMException("blocked", "SecurityError"); },
  setItem() { throw new DOMException("quota", "QuotaExceededError"); },
};

test("the first-open offer shows only with no stored key, no dismissal and no stream", () => {
  assert.equal(shouldOfferKey({ hasKey: false, dismissed: false }), true);
  assert.equal(shouldOfferKey({ hasKey: true, dismissed: false }), false, "a stored key hides the offer");
  assert.equal(shouldOfferKey({ hasKey: false, dismissed: true }), false, "Not now hides the offer for good");
  assert.equal(shouldOfferKey({ hasKey: false, dismissed: false, streaming: true }), false, "never over an active stream");
});

test("Not now is remembered on this browser through its storage flag", () => {
  const storage = memoryStorage();
  assert.equal(readOfferDismissed(storage), false, "a fresh browser has not dismissed the offer");
  assert.equal(writeOfferDismissed(storage), true);
  assert.equal(storage.items.get(OFFER_DISMISSED_KEY), "1");
  assert.equal(readOfferDismissed(storage), true);
  assert.equal(shouldOfferKey({ hasKey: false, dismissed: readOfferDismissed(storage) }), false);
});

test("storage that throws or is missing counts as not dismissed and never throws", () => {
  assert.equal(readOfferDismissed(throwingStorage), false);
  assert.equal(writeOfferDismissed(throwingStorage), false, "a refused write is reported, not thrown");
  assert.equal(readOfferDismissed(null), false);
  assert.equal(writeOfferDismissed(null), false);
  assert.equal(shouldOfferKey({ hasKey: false, dismissed: readOfferDismissed(throwingStorage) }), true,
    "without a working flag the offer still shows on the next load");
});

test("export file name uses the label, falls back to the ID, and drops characters Windows rejects", () => {
  assert.equal(exportFileName("Chrome on Windows", "B7QK-2M9X-H4PD-R8TN"), "Chrome on Windows.bpkey");
  assert.equal(exportFileName("", "B7QK-2M9X-H4PD-R8TN"), "B7QK-2M9X-H4PD-R8TN.bpkey");
  assert.equal(exportFileName(undefined, undefined), "device-key.bpkey");
  assert.equal(exportFileName("a/b:c*d?", "X"), "a b c d.bpkey");
  assert.equal(exportFileName("Pixel 8...", "X"), "Pixel 8.bpkey");
  assert.equal(exportFileName("..hidden", "X"), "hidden.bpkey");
  assert.equal(exportFileName("CON", "X"), "CON-key.bpkey");
  assert.equal(exportFileName("  Tab\tand\nnewline  ", "X"), "Tab and newline.bpkey");
});

test("a long export label is capped at 80 code points without splitting an emoji", () => {
  const name = exportFileName(`${"a".repeat(79)}\u{1F600}tail`, "X");
  assert.equal(name, `${"a".repeat(79)}\u{1F600}.bpkey`);
  assert.equal(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])/.test(name), false, "no lone surrogate");
});

test("the default label names the browser and the operating system", () => {
  const cases = [
    ["Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36", "Chrome on Windows"],
    ["Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36 Edg/130.0", "Edge on Windows"],
    ["Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15", "Safari on macOS"],
    ["Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0", "Firefox on Linux"],
    ["Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1", "Safari on iOS"],
    ["Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Mobile Safari/537.36", "Chrome on Android"],
  ];
  for (const [userAgent, label] of cases) assert.equal(defaultKeyLabel(userAgent), label);
  assert.equal(defaultKeyLabel(""), "Browser");
  assert.equal(defaultKeyLabel(), "Browser");
});

test("every module error code maps to a message that names the failure", () => {
  const expectations = {
    "authentication-failed": /Wrong passphrase/,
    "passphrase-required": /protected/,
    "identity-mismatch": /does not match/,
    "unsupported-version": /version/,
    "unsupported-type": /Ed25519/,
    "unsupported-format": /not a BubblePlay device key file/,
    "unsupported-kdf": /derivation/,
    "unsupported-aead": /encryption/,
    "invalid-id": /ID/,
    "invalid-file": /not a valid device key file/,
    "invalid-input": /cannot be used/,
  };
  for (const [code, pattern] of Object.entries(expectations)) {
    const message = deviceKeyErrorMessage({ code, message: "module detail" });
    assert.match(message, pattern, code);
    assert.notEqual(message, "module detail", `${code} uses our wording, not the module's`);
  }
});

test("errors without a known code show their own message, and anything else gets a generic one", () => {
  assert.equal(deviceKeyErrorMessage({ code: "something-new", message: "Device-key storage transaction aborted." }),
    "Device-key storage transaction aborted.");
  assert.equal(deviceKeyErrorMessage(new Error("IndexedDB is unavailable.")), "IndexedDB is unavailable.");
  assert.equal(deviceKeyErrorMessage(null), "Something went wrong.");
  assert.equal(deviceKeyErrorMessage("not an error"), "Something went wrong.");
  assert.equal(deviceKeyErrorMessage({ code: "authentication-failed", message: "" }), "Wrong passphrase, or the key file was changed.");
});

test("an empty passphrase field means no passphrase, and other text is passed through unchanged", () => {
  assert.equal(passphraseValue(""), undefined);
  assert.equal(passphraseValue(" "), " ", "spaces are part of the passphrase");
  assert.equal(passphraseValue("correct horse"), "correct horse");
});

test("a stored key is summarised as protected only when it has a KDF block", () => {
  assert.deepEqual(summarizeKeyFile({ id: "B7QK-2M9X-H4PD-R8TN", label: "Pixel 8", kdf: null }),
    { id: "B7QK-2M9X-H4PD-R8TN", label: "Pixel 8", protected: false });
  assert.deepEqual(summarizeKeyFile({ id: "B7QK-2M9X-H4PD-R8TN", label: "Chrome on Windows", kdf: { alg: "argon2id" } }),
    { id: "B7QK-2M9X-H4PD-R8TN", label: "Chrome on Windows", protected: true });
  assert.deepEqual(summarizeKeyFile({}), { id: "", label: "", protected: false });
});
