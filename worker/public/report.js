// Logs a failure once per event name for the life of the page, so a broken path that fails on
// every input or frame leaves one line with its cause instead of a flood or silence.
export function createOnceReporter(log) {
  const seen = new Set();
  return (event, error, fields = {}) => {
    if (seen.has(event)) return false;
    seen.add(event);
    log("warn", event, { error: String(error?.message ?? error).slice(0, 160), ...fields });
    return true;
  };
}
