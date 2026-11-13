// Secure-desktop Android composition: the text of a composition is committed once, through one
// pending transaction. Android commonly emits compositionend followed by one non-composing input
// event; that input consumes the pending transaction and the fallback timer then does nothing.
// Browsers that omit the trailing input still commit once, after this delay.
export const COMPOSITION_COMMIT_DELAY_MS = 50;

export function createCompositionCommitGate(commit, schedule = setTimeout, cancel = clearTimeout) {
  let pending = null;
  return {
    stage(text) {
      this.reset();
      const value = String(text ?? "");
      if (!value) return false;
      const transaction = { value, timer: null };
      transaction.timer = schedule(() => {
        if (pending !== transaction) return;
        pending = null;
        commit(transaction.value);
      }, COMPOSITION_COMMIT_DELAY_MS);
      pending = transaction;
      return true;
    },
    consume(text) {
      if (!pending) return false;
      const transaction = pending;
      pending = null;
      cancel(transaction.timer);
      commit(String(text ?? "") || transaction.value);
      return true;
    },
    reset() {
      if (pending) cancel(pending.timer);
      pending = null;
    },
  };
}
