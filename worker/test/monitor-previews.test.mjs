import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { PREVIEW_CAP, PREVIEW_LIMITS, createMonitorPreviews, createPreviewStore, parseMonitorPreview } from "../public/monitor-previews.js";
import { PREVIEWS_CAP, VIEWER_HELLO_CAPS } from "../public/dk-session.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/monitor-previews-contract.json", import.meta.url)));
const valid = contract.accept.find(v => v.name === "valid").json;

function fakeEl() { return { hidden: true, style: {}, src: null, removeAttribute(name) { if (name === "src") this.src = null; } }; }
function rig() {
  const el = fakeEl(), urls = new Set(), timers = new Map();
  let n = 0, t = 0;
  const previews = createMonitorPreviews({ el,
    createUrl: () => { const u = `blob:${++n}`; urls.add(u); return u; }, revokeUrl: u => urls.delete(u),
    makeBlob: bytes => bytes,
    schedule: (fn) => { timers.set(++t, { fn }); return t; }, cancel: id => timers.delete(id) });
  const fire = () => { for (const [id, { fn }] of [...timers]) { timers.delete(id); fn(); } };
  return { el, urls, timers, previews, fire };
}
const list = (...entries) => entries.map(([selector, selected]) => ({ selector, selected: !!selected }));
const msg = (monitor, seq) => ({ ...valid, monitor, seq });

test("the capability string matches the contract and is advertised by the viewer", () => {
  assert.equal(PREVIEW_CAP, contract.capability);
  assert.equal(PREVIEWS_CAP, contract.capability);
  assert.ok(VIEWER_HELLO_CAPS.includes(contract.capability));
  assert.equal(PREVIEW_LIMITS.maxJpegBytes, contract.limits.maxJpegBytes);
  assert.equal(PREVIEW_LIMITS.maxDimension, contract.limits.maxDimension);
});

test("accept vectors parse", () => {
  for (const { name, json } of contract.accept) {
    const picture = parseMonitorPreview(json);
    assert.ok(picture, name);
    assert.equal(picture.monitor, json.monitor);
    assert.equal(picture.seq, json.seq);
    assert.deepEqual([...picture.bytes.slice(0, 3)], [0xff, 0xd8, 0xff]);
  }
});

test("ignore vectors are ignored", () => {
  for (const { name, json, after } of contract.ignore) {
    if (after) continue; // the seq rule, below
    assert.equal(parseMonitorPreview(json), null, name);
  }
  assert.equal(parseMonitorPreview(null), null);
  assert.equal(parseMonitorPreview({ ...valid, seq: -1 }), null);
  assert.equal(parseMonitorPreview({ ...valid, seq: 1.5 }), null);
  assert.equal(parseMonitorPreview({ ...valid, w: 961 }), null);
  assert.equal(parseMonitorPreview({ ...valid, type: "other" }), null);
  const big = Buffer.alloc(PREVIEW_LIMITS.maxJpegBytes + 3, 1); big[0] = 0xff; big[1] = 0xd8; big[2] = 0xff;
  assert.equal(parseMonitorPreview({ ...valid, jpeg: big.toString("base64") }), null, "over the decoded size limit");
});

test("seq rule: an older seq after a newer one is ignored, per monitor", () => {
  const store = createPreviewStore();
  for (const { name, json, after } of contract.ignore.filter(v => v.after)) {
    assert.ok(store.hold(parseMonitorPreview(contract.accept.find(v => v.name === after).json)));
    assert.equal(store.hold(parseMonitorPreview(json)), false, name);
    assert.equal(store.get(valid.monitor).seq, valid.seq);
  }
  assert.ok(store.hold(parseMonitorPreview(msg("DISPLAY3", 1))), "another monitor has its own seq");
  assert.ok(store.hold(parseMonitorPreview(msg(valid.monitor, valid.seq + 1))));
  assert.equal(store.get(valid.monitor).seq, valid.seq + 1);
  assert.equal(store.size, 2, "one picture per monitor");
});

test("a held picture is shown at once when the selection changes, then fades on the first live frame", () => {
  const { el, urls, timers, previews, fire } = rig();
  assert.equal(previews.onMonitorList(list(["A", true], ["B"])), false, "first list only sets the live monitor");
  assert.equal(previews.onMessage(msg("B", 1)), true);
  assert.equal(previews.onMessage(msg("A", 1)), false, "the live monitor gets no held picture");
  assert.equal(previews.onMonitorList(list(["A"], ["B", true])), true);
  assert.equal(el.hidden, false);
  assert.equal(el.style.opacity, "1");
  assert.equal(el.style.transition, "none", "immediate");
  assert.equal(urls.size, 1);
  assert.equal(previews.store.get("B"), null, "the live monitor's picture is dropped");
  previews.onFirstFrame();
  assert.equal(el.style.opacity, "0");
  assert.match(el.style.transition, /200ms/);
  assert.equal(el.hidden, false, "still in place while fading");
  fire();
  assert.equal(el.hidden, true);
  assert.equal(urls.size, 0, "object URL revoked");
  assert.equal(timers.size, 0);
});

test("no picture held, unchanged selection, or a first frame without overlay: nothing happens", () => {
  const { el, previews } = rig();
  previews.onMonitorList(list(["A", true], ["B"]));
  assert.equal(previews.onMonitorList(list(["A"], ["B", true])), false);
  assert.equal(el.hidden, true);
  previews.onMessage(msg("A", 1));
  assert.equal(previews.onMonitorList(list(["A"], ["B", true])), false, "same selection again");
  previews.onFirstFrame();
  assert.equal(el.hidden, true);
  assert.equal(previews.onMonitorList([]), false);
});

test("a second switch replaces the overlay and revokes the old URL; the safety timer removes a stuck one", () => {
  const { el, urls, previews, fire } = rig();
  previews.onMonitorList(list(["A", true], ["B"], ["C"]));
  previews.onMessage(msg("B", 1)); previews.onMessage(msg("C", 1));
  previews.onMonitorList(list(["A"], ["B", true], ["C"]));
  previews.onMonitorList(list(["A"], ["B"], ["C", true]));
  assert.equal(urls.size, 1);
  fire(); // safety fires: starts the fade
  assert.equal(el.style.opacity, "0");
  fire();
  assert.equal(el.hidden, true);
  assert.equal(urls.size, 0);
});

test("reset (session end) drops every picture and the overlay", () => {
  const { el, urls, previews } = rig();
  previews.onMonitorList(list(["A", true], ["B"]));
  previews.onMessage(msg("B", 5));
  previews.onMonitorList(list(["A"], ["B", true]));
  previews.reset();
  assert.equal(el.hidden, true);
  assert.equal(urls.size, 0);
  assert.equal(previews.store.size, 0);
  previews.onMonitorList(list(["A", true], ["B"]));
  assert.equal(previews.onMessage(msg("B", 1)), true, "seq restarts with a new session");
});
