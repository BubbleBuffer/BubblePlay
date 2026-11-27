// Sticky modifiers of the on-screen keyboard. Each modifier is off, latched (held for the next key
// only) or locked (held until tapped again). A held modifier is pressed on the PC at once and
// released when it turns off, so the PC never sees a modifier the page forgot.

export const LONG_PRESS_MS = 400;
export const MODIFIER_CODES = Object.freeze({ 162: "ControlLeft", 164: "AltLeft", 160: "ShiftLeft", 91: "MetaLeft" });

// env: press(vk), release(vk), onChange(), schedule/cancel (optional, for tests).
export function createModifierLatch(env) {
  const schedule = env.schedule ?? setTimeout;
  const cancel = env.cancel ?? clearTimeout;
  const states = new Map(); // vk -> "latched" | "locked"
  const timers = new Map(); // vk -> { id, fired }

  const change = () => env.onChange?.();
  function set(vk, state) {
    if (!states.has(vk)) env.press(vk);
    states.set(vk, state);
  }
  function clear(vk) {
    if (!states.delete(vk)) return;
    env.release(vk);
  }

  const latch = {
    state: vk => states.get(vk) ?? "off",
    active: () => states.size > 0,
    held: () => [...states.keys()],
    /** Tap: off -> latched -> locked -> off. */
    tap(vk) {
      const state = states.get(vk);
      if (!state) set(vk, "latched");
      else if (state === "latched") set(vk, "locked");
      else clear(vk);
      change();
    },
    /** Long press: lock (a locked modifier stays locked). */
    lock(vk) { set(vk, "locked"); change(); },
    /** A key was sent: one-shot modifiers end, locks stay. */
    consumeOneShot() {
      let any = false;
      for (const [vk, state] of [...states]) if (state === "latched") { clear(vk); any = true; }
      if (any) change();
    },
    /** Hide, blur, disconnect, session end and Clear. */
    releaseAll() {
      for (const timer of timers.values()) cancel(timer.id);
      timers.clear();
      const any = states.size > 0;
      for (const vk of [...states.keys()]) clear(vk);
      if (any) change();
    },
    /** The input transport already released every key: forget without sending releases. */
    forget() {
      for (const timer of timers.values()) cancel(timer.id);
      timers.clear();
      if (states.size) { states.clear(); change(); }
    },
    pointerDown(vk) {
      const old = timers.get(vk);
      if (old) cancel(old.id);
      const entry = { fired: false, id: null };
      entry.id = schedule(() => { entry.fired = true; latch.lock(vk); }, env.longPressMs ?? LONG_PRESS_MS);
      timers.set(vk, entry);
    },
    pointerUp(vk) {
      const entry = timers.get(vk);
      if (!entry) return;
      cancel(entry.id);
      timers.delete(vk);
      if (!entry.fired) latch.tap(vk);
    },
    pointerCancel(vk) {
      const entry = timers.get(vk);
      if (entry) { cancel(entry.id); timers.delete(vk); }
    },
    /**
     * Runs a preset chord that ignores one-shot modifiers and leaves locks alone: latched modifiers
     * and locked ones the chord also presses are lifted first, then put back after it.
     */
    withChord(codes, run) {
      const lifted = [...states].filter(([vk, state]) => state === "latched" || codes.includes(MODIFIER_CODES[vk])).map(([vk]) => vk);
      for (const vk of lifted) env.release(vk);
      try { return run(); } finally { for (const vk of lifted) env.press(vk); }
    },
  };
  return latch;
}
