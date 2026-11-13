// vm harnesses that run slices of receiver.js. The slices read the shared page state through
// `sessionState`; this builds it from the fields a test sets, and keeps `sandbox.<field>` readable
// and writable on the same object, so a test says what it means ("usingWssMedia: true").
import vm from "node:vm";
import { createSessionState } from "../../public/session-state.js";

const SESSION_FIELDS = Object.keys(createSessionState({ wssSelected: false }));

// Adds `sessionState` to the sandbox object (in place) and returns it.
export function sessionSandbox(sandbox = {}) {
  const sessionState = createSessionState({ wssSelected: false });
  for (const key of SESSION_FIELDS) {
    if (Object.hasOwn(sandbox, key)) { sessionState[key] = sandbox[key]; delete sandbox[key]; }
  }
  sandbox.sessionState = sandbox.sessionState ?? sessionState;
  for (const key of SESSION_FIELDS) {
    Object.defineProperty(sandbox, key, {
      get: () => sandbox.sessionState[key], set: value => { sandbox.sessionState[key] = value; },
      enumerable: false, configurable: true,
    });
  }
  return sandbox;
}

export function createSessionContext(sandbox = {}) {
  return vm.createContext(sessionSandbox(sandbox));
}
