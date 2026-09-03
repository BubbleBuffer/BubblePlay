import test from "node:test"; import assert from "node:assert/strict";
import { canMoveAbsoluteCursor, cursorDriftPixels, InputController, packet, reconcileTextEdit, RemoteCursor, secureTextUnits, shouldAcceptSequence, shouldCaptureKeyboard, shouldCorrectCursorDrift, shouldShowCursorOverlay, shouldShowPredictedCursor, TouchGesture, Viewport } from "../public/input.js";
import * as controls from "../public/input.js";
test("encodes v2 and accepts only wrap-safe sequences", () => { assert.deepEqual([...packet.motion(0xffff,-2,3,120,-120)],[7,255,255,254,255,3,0,120,0,136,255]); assert.deepEqual([...packet.absolute(4,12,34)],[8,4,0,12,0,34,0]); assert.deepEqual([...packet.buttonsAt(5,65535,32768,1)],[9,5,0,255,255,0,128,1]); assert.equal(shouldAcceptSequence(0xffff,0),true); assert.equal(shouldAcceptSequence(0,0xffff),false); });
test("encodes bounded UTF-8 text packets and rejects oversize text", () => { assert.deepEqual([...packet.text("hé")],[11,104,195,169]); assert.equal(packet.text(""),null); assert.equal(packet.text("x".repeat(513)),null); const sent=[]; const c=new InputController({sendControl:p=>(sent.push([...p]),true)}); assert.equal(c.text("hello"),true); assert.deepEqual(sent[0].slice(0,2),[11,104]); });
test("desktop keyboard works over the remote surface without hijacking local controls", () => {
  const base = { inputReady:true, pointerMode:"desktop", locked:false, surfaceFocused:false,
    keyboardFocused:false, localControlFocused:false };
  assert.equal(shouldCaptureKeyboard(base), true);
  assert.equal(shouldCaptureKeyboard({ ...base, localControlFocused:true }), false);
  assert.equal(shouldCaptureKeyboard({ ...base, keyboardFocused:true, localControlFocused:true }), true);
  assert.equal(shouldCaptureKeyboard({ ...base, inputReady:false }), false);
  assert.equal(shouldCaptureKeyboard({ ...base, pointerMode:"game" }), false);
  assert.equal(shouldCaptureKeyboard({ ...base, pointerMode:"game", locked:true }), true);
});
test("reconciles append, deletion, autocorrect, paste, and middle edits", () => {
  assert.deepEqual(reconcileTextEdit("", "yor"), { deleteCount: 0, insert: "yor" });
  assert.deepEqual(reconcileTextEdit("yor", "you're"), { deleteCount: 1, insert: "u're" });
  assert.deepEqual(reconcileTextEdit("hello", "hell"), { deleteCount: 1, insert: "" });
  assert.deepEqual(reconcileTextEdit("hello world", "hello brave world"), { deleteCount: 5, insert: "brave world" });
  assert.deepEqual(reconcileTextEdit("foo", "foobar"), { deleteCount: 0, insert: "bar" });
  assert.deepEqual(reconcileTextEdit("dont", "don't"), { deleteCount: 1, insert: "'t" });
});
test("reconciliation treats Unicode code points as single editable characters", () => {
  assert.deepEqual(reconcileTextEdit("🙂a", "🙂"), { deleteCount: 1, insert: "" });
  assert.deepEqual(reconcileTextEdit("café", "café"), { deleteCount: 1, insert: "é" });
});
test("secure text accepts explicit typing one scalar at a time and rejects paste", () => {
  assert.deepEqual(secureTextUnits("a🙂", "insertText"), ["a", "🙂"]);
  assert.deepEqual(secureTextUnits("é", "insertCompositionText"), ["é"]);
  assert.deepEqual(secureTextUnits("secret", "insertFromPaste"), []);
  assert.deepEqual(secureTextUnits("secret", "insertReplacementText"), []);
});
test("absolute coalesces and relative/absolute supersede stale motion", () => { const sent=[]; const c=new InputController({sendMotion:p=>(sent.push([...p]),true)}); c.absolute(1,2); c.absolute(3,4); c.flushMotion(); assert.deepEqual(sent.at(-1),[8,0,0,3,0,4,0]); c.motion(5,6); c.absolute(7,8); c.flushMotion(); assert.equal(sent.at(-1)[0],8); c.absolute(7,8); c.motion(9,10); c.flushMotion(); assert.equal(sent.at(-1)[0],7); });
test("motion sender rejection and bounded backlog drop without advancing sequence", () => { const c=new InputController({sendMotion:()=>false}); c.motion(1,2); assert.equal(c.flushMotion(),false); assert.equal(c.stats.dropped,1); assert.equal(c.seq,0); const b=new InputController({sendMotion:()=>true,motionBuffered:()=>65}); b.motion(1,1); b.flushMotion(); assert.equal(b.stats.dropped,1); const ok=new InputController({sendMotion:()=>true,motionBuffered:()=>64}); ok.motion(1,1); assert.equal(ok.flushMotion(),true); });
test("release clears held buttons and pending motion", () => { const sent=[]; const c=new InputController({sendControl:p=>(sent.push([...p]),true)}); c.key(65,false); c.buttons(1); c.motion(2,2); c.releaseAll(); assert.deepEqual(sent.at(-1),[6]); assert.equal(c.held.size,0); assert.equal(c.buttonMask,0); assert.deepEqual(c.pending,[0,0,0,0]); });
test("reliable input telemetry distinguishes accepted and rejected sends", () => {
  let accept = true;
  const c = new InputController({ sendControl: () => accept });
  assert.equal(c.key(65, false), true);
  accept = false;
  assert.equal(c.buttons(1), false);
  assert.equal(c.stats.controlSent, 1);
  assert.equal(c.stats.controlDropped, 1);
});
test("reliable click warp consumes a motion barrier and clears older pending motion", () => { const sent=[]; const c=new InputController({sendControl:p=>(sent.push([...p]),true)}); c.motion(9,-4); assert.equal(c.buttonsAt(65535,32768,1),true); assert.deepEqual(sent,[ [9,0,0,255,255,0,128,1] ]); assert.equal(c.seq,1); assert.equal(c.buttonMask,1); assert.equal(c.flushMotion(),false); });
test("viewport retains midpoint content and clamps pan/reset", () => { const v=new Viewport(100,100); v.setZoom(2,{x:25,y:25}); assert.deepEqual(v.snapshot(),{zoom:2,x:-25,y:-25}); v.pan(-1000,1000); assert.deepEqual(v.snapshot(),{zoom:2,x:-100,y:0}); v.setZoom(1); assert.deepEqual(v.snapshot(),{zoom:1,x:0,y:0}); });
test("viewport clamps the contained media rect, never the wrapper", () => { const v=new Viewport(100,177,{x:0,y:60.375,width:100,height:56.25}); v.setZoom(4,{x:50,y:88}); v.pan(-999,999); const s=v.snapshot(); assert.ok(s.y >= -289.5 && s.y <= -241.5); v.setZoom(1); assert.deepEqual(v.snapshot(),{zoom:1,x:0,y:0}); });
test("second touch explicitly suppresses queued one-finger motion", () => { const queued=[]; const c=new InputController({sendMotion:p=>(queued.push([...p]),true)}), g=new TouchGesture(); g.down(1,{x:0,y:0}); c.motion(4,0); assert.deepEqual(g.down(2,{x:10,y:0}),[{type:"suppress-motion"}]); c.clearMotion(); assert.equal(c.flushMotion(),false); assert.deepEqual(queued,[]); });
test("touch drag has motion, a final settle, and never a tap", () => { const g=new TouchGesture(); g.down(1,{x:0,y:0}); assert.deepEqual(g.move(1,{x:9,y:0}),[{type:"motion",dx:9,dy:0}]); assert.deepEqual(g.up(1),[{type:"motion-end"}]); });
test("touch locks slow cumulative pinch or translate and cancellation waits for all contacts", () => { const pinch=new TouchGesture(); pinch.down(1,{x:0,y:0}); pinch.down(2,{x:100,y:0}); pinch.move(2,{x:103,y:0}); assert.deepEqual(pinch.move(2,{x:106,y:0}).map(x=>x.type),["pinch"]); const pan=new TouchGesture({pinchThreshold:100}); pan.down(1,{x:0,y:0}); pan.down(2,{x:100,y:0}); pan.move(1,{x:3,y:0}); assert.deepEqual(pan.move(1,{x:16,y:0}).map(x=>x.type),["translate"]); pan.cancel(1); assert.equal(pan.cancelled,true); assert.deepEqual(pan.move(2,{x:101,y:0}),[]); pan.up(2); assert.equal(pan.cancelled,false); });
test("second tap holds left for drag and releases exactly once", () => { const g=new TouchGesture(); g.down(1,{x:10,y:10},0); assert.deepEqual(g.up(1,40),[{type:"tap"}]); assert.deepEqual(g.down(2,{x:14,y:12},180),[{type:"drag-start"}]); assert.deepEqual(g.move(2,{x:24,y:12}).map(x=>x.type),["motion"]); assert.deepEqual(g.up(2,230),[{type:"drag-end"}]); assert.deepEqual(g.up(2,240),[]); });
test("two-finger tap is a right click, not a left tap", () => { const g=new TouchGesture(); g.down(1,{x:10,y:10},0); assert.deepEqual(g.down(2,{x:30,y:10},20),[{type:"suppress-motion"}]); assert.deepEqual(g.up(1,80),[]); assert.deepEqual(g.up(2,90),[{type:"right-tap"}]); });
test("predicted cursor maps through pan/zoom and stays the zoom anchor", () => { const v=new Viewport(200,100,{x:20,y:10,width:160,height:80}), c=new RemoteCursor(); assert.deepEqual(c.screenPoint(v),{x:100,y:50}); c.moveScreen(32,-8,v); assert.deepEqual(c.snapshot(),{x:0.7,y:0.4}); const anchor=c.screenPoint(v); v.setZoom(2,anchor); assert.deepEqual(c.screenPoint(v),anchor); c.setFromScreen({x:-1000,y:1000},v); assert.deepEqual(c.snapshot(),{x:0,y:1}); });
test("zoomed camera follows the cursor to center and clamps at desktop edges", () => { const v=new Viewport(200,100), c=new RemoteCursor(.7,.4); v.setZoom(2); v.centerOnNormalized(c.snapshot()); assert.deepEqual(c.screenPoint(v),{x:100,y:50}); c.setNormalized(.05,.05); v.centerOnNormalized(c.snapshot()); assert.deepEqual(c.screenPoint(v),{x:20,y:10}); });
test("predicted cursor converts to the absolute Windows coordinate range", () => { const c=new RemoteCursor(.25,.75); assert.deepEqual(c.absolutePoint(),{x:16384,y:49151}); });
test("absolute diagnostic cursor movement requires ready Work input but not prior feedback sync", () => {
  assert.equal(canMoveAbsoluteCursor({ pointerMode:"desktop", inputReady:true, synced:false }, .2, .8), true);
  assert.equal(canMoveAbsoluteCursor({ pointerMode:"desktop", inputReady:false, synced:true }, .2, .8), false);
  assert.equal(canMoveAbsoluteCursor({ pointerMode:"game", inputReady:true, synced:true }, .2, .8), false);
  assert.equal(canMoveAbsoluteCursor({ pointerMode:"desktop", inputReady:true, synced:true }, -0.1, .8), false);
  assert.equal(canMoveAbsoluteCursor({ pointerMode:"desktop", inputReady:true, synced:true }, .2, 1.1), false);
});
test("locally authoritative cursor follows at 2x before host feedback", () => {
  const viewport = new Viewport(200, 100), cursor = new RemoteCursor();
  viewport.setZoom(2, { x:100, y:50 });
  cursor.setNormalized(.95, .5);
  const shouldFollow = controls.shouldCenterCameraOnCursor({
    zoom: viewport.zoom,
    pointerMode: "desktop",
    synced: false,
    locallyAuthoritative: true,
  });
  assert.equal(shouldFollow, true);
  if (shouldFollow) viewport.centerOnNormalized(cursor.snapshot());
  assert.deepEqual(viewport.snapshot(), { zoom:2, x:-200, y:-50 });
  assert.deepEqual(cursor.screenPoint(viewport), { x:180, y:50 });
  assert.equal(controls.shouldCenterCameraOnCursor({
    zoom: viewport.zoom,
    pointerMode: "desktop",
    synced: false,
    locallyAuthoritative: false,
  }), false);
});
test("touchpad gain maps a finger delta to the same visible remote distance", () => { const v=new Viewport(200,100,{x:0,y:0,width:200,height:100}), c=new RemoteCursor(); v.setZoom(2,{x:100,y:50}); assert.deepEqual(c.remoteDeltaForScreen(10,-5,v,{width:800,height:400}),{dx:20,dy:-10}); });
test("touch motion flushes immediately through bounded motion sender", () => { const sent=[]; const c=new InputController({sendMotion:p=>(sent.push([...p]),true)}); assert.equal(c.motionNow(3,4),true); assert.equal(sent.length,1); assert.deepEqual(sent[0].slice(0,7),[7,0,0,3,0,4,0]); });
test("cancelling an active double-tap drag releases it once", () => { const g=new TouchGesture(); g.down(1,{x:0,y:0},0); g.up(1,20); g.down(2,{x:0,y:0},100); assert.deepEqual(g.cancel(2),[{type:"drag-end"}]); assert.deepEqual(g.cancel(2),[]); });

test("pointer sync barrier is reliable and consumes the next motion sequence", () => {
  assert.equal(typeof packet.cursorSync, "function");
  const sent=[];
  const c=new InputController({sendControl:p=>(sent.push([...p]),true)});
  c.motion(12,-3);
  assert.equal(c.cursorSync(),true);
  assert.deepEqual(sent,[[10,0,0]]);
  assert.equal(c.seq,1);
  assert.equal(c.flushMotion(),false);
});

test("a completed one-finger move requests a reliable final-position settle", () => {
  const g=new TouchGesture();
  g.down(1,{x:0,y:0});
  g.move(1,{x:12,y:0});
  assert.deepEqual(g.up(1),[{type:"motion-end"}]);
});

test("cursor feedback never rewinds a newer local absolute target", () => {
  assert.equal(typeof controls.decodeCursorState, "function");
  assert.equal(typeof controls.CursorReconciler, "function");
  const cursor=new RemoteCursor();
  const sync=new controls.CursorReconciler(cursor);
  sync.noteSent(7);
  const stale=controls.decodeCursorState(Uint8Array.of(128,6,0,0,0,0,0,7));
  assert.equal(sync.apply(stale,{touchActive:true}),false);
  assert.equal(sync.synced,false);
  assert.deepEqual(cursor.snapshot(),{x:.5,y:.5});
  const current=controls.decodeCursorState(Uint8Array.of(128,7,0,0,64,0,192,7));
  assert.equal(sync.apply(current,{touchActive:false}),true);
  assert.deepEqual(cursor.snapshot(),{x:16384/65535,y:49152/65535});
  assert.equal(sync.pending,false);
});

test("authoritative cursor overlay is available on fine-pointer laptops too", () => {
  assert.equal(shouldShowCursorOverlay({ pointerMode:"desktop", synced:true, visible:true, inBounds:true }), true);
  assert.equal(shouldShowCursorOverlay({ pointerMode:"desktop", synced:true, visible:false, inBounds:true }), true);
  assert.equal(shouldShowCursorOverlay({ pointerMode:"game", synced:true, visible:true, inBounds:true }), false);
  assert.equal(shouldShowCursorOverlay({ pointerMode:"desktop", synced:false, visible:true, inBounds:true }), false);
  assert.equal(shouldShowPredictedCursor({ pointerMode:"desktop", inputReady:true }), true);
  assert.equal(shouldShowPredictedCursor({ pointerMode:"game", inputReady:true }), false);
});

test("hidden Windows cursor feedback still updates authoritative overlay position", () => {
  const cursor=new RemoteCursor(), sync=new controls.CursorReconciler(cursor);
  sync.noteSent(3);
  assert.equal(sync.apply(controls.decodeCursorState(Uint8Array.of(128,3,0,0,64,0,192,3))),true);
  assert.deepEqual(cursor.snapshot(),{x:16384/65535,y:49152/65535});
});

test("work cursor corrects meaningful drift at a bounded cadence while gaming never warps", () => {
  const cursor=new RemoteCursor(.5,.5), remote={width:1920,height:1080};
  const state={x:Math.round(.6*65535),y:Math.round(.5*65535),inBounds:true};
  assert.ok(cursorDriftPixels(state,cursor,remote)>190);
  const base={pointerMode:"desktop",locallyAuthoritative:true,state,cursor,remote,now:200,lastCorrectionAt:0};
  assert.equal(shouldCorrectCursorDrift(base),true);
  assert.equal(shouldCorrectCursorDrift({...base,now:50}),false);
  assert.equal(shouldCorrectCursorDrift({...base,pointerMode:"game"}),false);
  assert.equal(shouldCorrectCursorDrift({...base,locallyAuthoritative:false}),false);
});

test("work cursor feedback can acknowledge without overwriting its local target", () => {
  const cursor=new RemoteCursor(.75,.25), sync=new controls.CursorReconciler(cursor);
  sync.noteSent(9);
  const state=controls.decodeCursorState(Uint8Array.of(128,9,0,0,64,0,192,7));
  assert.equal(sync.apply(state,{preserveCursor:true}),true);
  assert.deepEqual(cursor.snapshot(),{x:.75,y:.25});
  assert.equal(sync.pending,false);
});
