// A recovery tracker that never recovers, for vm harnesses that slice receiver.js functions.
export function recoveryStub(over = {}) {
  return {
    recovering: false, attempts: 0, noteLoss() {}, noteAuthenticated() {}, noteConnectionOpened() {},
    authTimeoutMs: () => undefined, connectTimeoutMs: () => null, shouldRetryAuthTimeout: () => false,
    noteRecovered() {}, sinceLossMs: () => null, tick() {}, freeze() {}, resume() {}, ...over,
  };
}
