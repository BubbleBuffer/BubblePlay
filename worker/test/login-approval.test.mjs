import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";

const html = await readFile(new URL("../public/login.html", import.meta.url), "utf8");
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];

async function page(methods, { offline = false, fail = false } = {}) {
  const elements = new Map();
  for (const id of [...html.matchAll(/id="([^"]+)"/g)].map(match => match[1])) {
    elements.set(id, { hidden: ["approval-options", "request-approval"].includes(id),
      textContent: "", disabled: false, value: "password", checked: true,
      focus() {}, select() {}, addEventListener(type, handler) { this[type] = handler; } });
  }
  const button = { disabled: false, textContent: "Open BubblePlay" };
  elements.get("login").querySelector = () => button;
  const calls = [];
  const location = { href: "https://play.test/?approval=request&transport=wss&profile=1",
    assign(value) { this.assigned = value; },
    replace(value) { this.replaced = value; } };
  vm.runInNewContext(script, {
    document: { body: { hasAttribute: () => offline }, getElementById: id => elements.get(id) },
    location, URL,
    fetch: async (url, options) => {
      calls.push({ url, options });
      if (url === "/__auth/login") return { ok: true };
      if (fail) throw new Error("offline");
      return { ok: true, json: async () => methods };
    },
  });
  await new Promise(resolve => setImmediate(resolve));
  return { elements, calls, location, button };
}

test("login discovers approval methods without caching or sending secrets", async () => {
  const p = await page({ totp: false, approval: ["ntfy"], hostOnline: false });
  assert.equal(p.calls[0].url, "/__auth/methods");
  assert.equal(p.calls[0].options.cache, "no-store");
  assert.equal(p.calls[0].options.credentials, "same-origin");
  assert.equal(p.elements.get("request-approval").hidden, false);
  assert.match(p.elements.get("approval-destination").textContent, /ntfy notifications/);
  p.elements.get("request-approval").click();
  assert.equal(p.location.assigned, "/?approval=request");
  assert.match(html, /id="request-approval" type="button" hidden>Request approval instead/);
});

test("both methods explain fanout and PC unavailability", async () => {
  const online = await page({ totp: false, approval: ["ntfy", "host"], hostOnline: true });
  assert.match(online.elements.get("approval-destination").textContent, /ntfy notifications and.*PC.*first decision wins/);
  const offline = await page({ totp: false, approval: ["ntfy", "host"], hostOnline: false });
  assert.match(offline.elements.get("approval-destination").textContent, /PC is unreachable/);
  assert.equal(offline.elements.get("request-approval").hidden, false);
});

test("host-only offline is clear and preserves already approved password login", async () => {
  const p = await page({ totp: false, approval: ["host"], hostOnline: false });
  assert.match(p.elements.get("approval-destination").textContent, /PC is unreachable.*reload/);
  assert.equal(p.elements.get("request-approval").hidden, true);
  assert.equal(p.elements.get("login").hidden, false);
});

test("fresh host-only offline page cannot claim browser approval", async () => {
  const p = await page({ totp: false, approval: ["host"], hostOnline: false }, { offline: true });
  assert.equal(p.elements.get("login").hidden, true);
  assert.equal(p.elements.get("approval-state").textContent, "Approval required");
  assert.match(p.elements.get("approval-proof").textContent, /needs approval/);
  assert.match(p.elements.get("login-intro").textContent, /PC is unreachable/);
});

test("host-only online names the dialog on the PC", async () => {
  const p = await page({ totp: false, approval: ["host"], hostOnline: true });
  assert.match(p.elements.get("approval-destination").textContent, /dialog on your PC/);
  assert.equal(p.elements.get("request-approval").hidden, false);
});

test("failed or malformed discovery does not offer unconfigured approval", async () => {
  for (const methods of [null, { totp: false, approval: [], hostOnline: false },
    { totp: false, approval: ["bogus"], hostOnline: true },
    { totp: false, approval: ["host"], hostOnline: "yes" }]) {
    const p = await page(methods);
    assert.equal(p.elements.get("approval-options").hidden, true);
    assert.equal(p.elements.get("request-approval").hidden, true);
    assert.equal(p.button.disabled, false);
  }
  const failed = await page(null, { fail: true });
  assert.equal(failed.elements.get("approval-options").hidden, true);
});

test("password login consumes approval action while retaining transport and profile", async () => {
  const p = await page({ totp: false, approval: ["ntfy"], hostOnline: false });
  await p.elements.get("login").submit({ preventDefault() {} });
  assert.equal(p.location.replaced, "/?transport=wss&profile=1");
  assert.equal(p.calls[1].url, "/__auth/login");
  assert.deepEqual(JSON.parse(p.calls[1].options.body), { password: "password", remember: true });
});
