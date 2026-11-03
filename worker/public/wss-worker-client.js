// Main-thread side of the WSS decode worker: creates the worker, hands it the
// canvases (transferControlToOffscreen), and relays messages. Everything that
// is receiver state (recovery, HUD, ACKs) stays in receiver.js; this module only
// owns the worker's lifetime and the one-shot capability/transfer handshake.

/**
 * Pure feature check for the worker path. `scope` is the page's global
 * (injectable for tests); the worker repeats the probe for what only it can see.
 */
export function wssWorkerSupported(scope = globalThis) {
  try {
    return typeof scope.Worker === "function" &&
      typeof scope.VideoDecoder === "function" &&
      typeof scope.OffscreenCanvas === "function" &&
      typeof scope.HTMLCanvasElement?.prototype?.transferControlToOffscreen === "function";
  } catch { return false; }
}

export function createWssWorkerClient({
  createWorker = () => new Worker("/wss-worker.js", { type: "module" }),
  onMessage = () => {},
  onFatal = () => {},
  timeoutMs = 4_000,
} = {}) {
  let worker = null;
  let transferred = false;
  let ready = false;

  function stop() {
    ready = false;
    try { worker?.terminate(); } catch { /* already gone */ }
    worker = null;
  }

  /**
   * @returns {Promise<{ok: boolean, transferred: boolean, reason?: string}>}
   * `transferred` tells the caller whether it must replace the canvas element(s)
   * before drawing on the main thread again.
   */
  function start({ canvases, epoch, codec, timeOrigin = performance.timeOrigin }) {
    return new Promise((resolve) => {
      let settled = false;
      const settle = (result) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        if (!result.ok) stop();
        resolve({ ...result, transferred });
      };
      const timer = setTimeout(() => settle({ ok: false, reason: "worker-timeout" }), timeoutMs);
      try { worker = createWorker(); } catch { settle({ ok: false, reason: "worker-create" }); return; }
      const current = worker;
      current.onerror = (event) => {
        if (current !== worker) return;
        if (!settled) settle({ ok: false, reason: `worker-error:${event?.message || "load"}` });
        else onFatal(`worker-error:${event?.message || "error"}`);
      };
      current.onmessage = ({ data }) => {
        if (current !== worker) return;
        if (!settled) {
          if (data.type === "capabilities") {
            if (!data.supported) { settle({ ok: false, reason: "worker-unsupported" }); return; }
            try {
              const offscreen = {};
              for (const [name, canvas] of Object.entries(canvases)) offscreen[name] = canvas.transferControlToOffscreen();
              transferred = true;
              current.postMessage({ type: "init", ...offscreen, epoch, codec, timeOrigin }, Object.values(offscreen));
            } catch { settle({ ok: false, reason: "transfer-failed" }); }
            return;
          }
          if (data.type === "worker-ready") { ready = true; settle({ ok: true }); return; }
          if (data.type === "worker-init-error" || data.type === "worker-error") {
            settle({ ok: false, reason: data.message || data.type });
            return;
          }
        }
        onMessage(data);
      };
      current.postMessage({ type: "capabilities" });
    });
  }

  return {
    start,
    stop,
    get ready() { return ready; },
    get transferred() { return transferred; },
    post(message, transfer) { if (worker && ready) worker.postMessage(message, transfer || []); },
  };
}
