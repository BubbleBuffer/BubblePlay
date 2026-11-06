// Temporary pointer lock for a window drag that crosses monitors in Work mode.
//
// The host carries the cursor across an edge switch, but a page cannot move the
// real pointer, so it stops at the screen edge. Locking the pointer lets the
// user keep moving in the same direction: the lock turns further motion into
// relative deltas the host applies from its translated cursor position.
//
// This is only the state machine. The receiver supplies the browser calls, so
// every path (grant, refusal, every way the drag can end) is unit tested. The
// one rule that matters most: the button release always reaches the host.

export const DRAG_LOCK_IDLE = "idle";
export const DRAG_LOCK_REQUESTING = "requesting";
export const DRAG_LOCK_LOCKED = "locked";

/**
 * @param {object} deps
 * @param {() => number} deps.now                   monotonic ms
 * @param {() => (Promise<unknown>|unknown)} deps.requestLock   requestPointerLock() without unadjustedMovement
 * @param {() => void} deps.exitLock                exitPointerLock() when this lock is held
 * @param {() => void} deps.releaseButtons          sends the release to the host (releaseInput)
 * @param {(detail: object) => void} deps.log       one info line per attempt
 * @param {(locked: boolean) => void} [deps.onLockedChange]  drawn cursor on/off
 */
export function createDragLock({ now, requestLock, exitLock, releaseButtons, log, onLockedChange = () => {} }) {
  let phase = DRAG_LOCK_IDLE;
  let leftDownAt = null;   // ms of a mouse left-down in Work mode, null once released or not eligible
  let refused = false;     // a refusal ends the attempts for this drag
  let requestedAt = null;
  let attempt = 0;

  const setPhase = next => {
    const wasLocked = phase === DRAG_LOCK_LOCKED;
    phase = next;
    const isLocked = phase === DRAG_LOCK_LOCKED;
    if (wasLocked !== isLocked) onLockedChange(isLocked);
  };

  function refuse(error) {
    if (phase !== DRAG_LOCK_REQUESTING) return;
    refused = true;
    log({ granted: false, sinceDownMs: requestedAt, error: error || "unknown" });
    setPhase(DRAG_LOCK_IDLE);
  }

  function grant() {
    if (phase !== DRAG_LOCK_REQUESTING) return;
    log({ granted: true, sinceDownMs: requestedAt });
    if (leftDownAt === null) { // released while the request was pending
      setPhase(DRAG_LOCK_IDLE);
      exitLock();
      return;
    }
    setPhase(DRAG_LOCK_LOCKED);
  }

  // Leave the lock after our own end of drag. Never sends a release itself.
  function drop() {
    const hadLock = phase !== DRAG_LOCK_IDLE;
    setPhase(DRAG_LOCK_IDLE);
    if (hadLock) exitLock();
  }

  return {
    get phase() { return phase; },
    /** True while the lock is held: motion arrives as relative deltas. */
    locked: () => phase === DRAG_LOCK_LOCKED,

    /** A mouse button went down. Only a left press by a real mouse in Work mode can start a drag. */
    down({ button, pointerMode, touchLike }) {
      if (button !== 0) return;
      drop(); // a new press never inherits a stale request
      refused = false;
      leftDownAt = pointerMode === "desktop" && !touchLike ? now() : null;
    },

    /** The left button went up (or any button: a drag only ends with the left one). */
    up({ button }) {
      if (button !== 0) return;
      leftDownAt = null;
      // The receiver's own mouse-up path sends the release; the lock only has to go.
      drop();
    },

    /** A monitor switch arrived. Starts the lock only for a held left button in Work mode. */
    monitorSwitch({ pointerMode }) {
      if (phase !== DRAG_LOCK_IDLE || refused) return false;
      if (leftDownAt === null || pointerMode !== "desktop") return false;
      attempt++;
      requestedAt = Math.round(now() - leftDownAt);
      setPhase(DRAG_LOCK_REQUESTING);
      let result;
      try { result = requestLock(); } catch (error) { refuse(error?.name || String(error)); return true; }
      if (result && typeof result.then === "function") {
        result.then(grant, error => refuse(error?.name || String(error)));
      }
      return true;
    },

    /** pointerlockchange: `held` is whether our element holds the lock now. */
    lockChanged(held) {
      if (held) { grant(); return; }
      if (phase === DRAG_LOCK_LOCKED) { // Esc or the browser took the lock: the button is still down on the host
        setPhase(DRAG_LOCK_IDLE);
        leftDownAt = null;
        releaseButtons();
      }
    },

    /** pointerlockerror. */
    lockError(error) { refuse(error || "pointerlockerror"); },

    /**
     * The receiver is releasing all input anyway (blur, hidden tab, mode change, disconnect,
     * session end): just leave the lock. The caller sends the release.
     */
    stop() { leftDownAt = null; drop(); },

    get attempts() { return attempt; },
  };
}
