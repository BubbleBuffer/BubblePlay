// The same module is the browser entrypoint and the per-derivation Worker.
// Importing it on the main thread never loads or runs the Argon2 WASM build.
const TIMEOUT_MS = 120_000;

function inputs(password, salt) {
  if (!(password instanceof Uint8Array) || password.length === 0 || !(salt instanceof Uint8Array) || salt.length !== 16) {
    throw new TypeError("Argon2id requires nonempty UTF-8 password bytes and a 16-byte salt.");
  }
}

function wipe(bytes) {
  // Transferred buffers are detached; their receiver owns erasure from then on.
  if (bytes?.byteLength) bytes.fill(0);
}

/** Derive a 32-byte key off the UI thread; callers own and must erase the result. */
export async function deriveKeyInWorker({ password, salt }) {
  inputs(password, salt);
  if (typeof Worker !== "function") throw new Error("Device-key Argon2id requires Web Worker support.");
  const passwordCopy = password.slice(), saltCopy = salt.slice();
  let worker, timer;
  try {
    worker = new Worker(new URL("./devicekey-kdf-worker.js", import.meta.url), {
      type: "module", name: "bubbleplay-device-key-kdf",
    });
    return await new Promise((resolve, reject) => {
      worker.onmessage = ({ data }) => {
        if (data?.key instanceof Uint8Array && data.key.length === 32) resolve(data.key);
        else {
          wipe(data?.key instanceof Uint8Array ? data.key : null);
          reject(new Error(data?.error || "Invalid Argon2id Worker response."));
        }
      };
      worker.onerror = event => {
        event.preventDefault();
        reject(new Error("Device-key Argon2id Worker failed to load or run (check Worker/WASM policy)."));
      };
      worker.onmessageerror = () => reject(new Error("Cannot read Argon2id Worker response."));
      timer = setTimeout(() => reject(new Error("Device-key Argon2id Worker timed out.")), TIMEOUT_MS);
      // Copy before transfer so injected and browser KDFs share the same input
      // ownership: the caller can still erase its own buffers in a finally block.
      worker.postMessage({ password: passwordCopy, salt: saltCopy }, [passwordCopy.buffer, saltCopy.buffer]);
    });
  } finally {
    clearTimeout(timer);
    worker?.terminate();
    wipe(passwordCopy);
    wipe(saltCopy);
  }
}

if (typeof WorkerGlobalScope !== "undefined" && globalThis instanceof WorkerGlobalScope) {
  globalThis.onmessage = async ({ data }) => {
    let key;
    try {
      inputs(data?.password, data?.salt);
      await import("./vendor/hash-wasm/argon2.umd.min.js");
      key = await globalThis.hashwasm.argon2id({
        password: data.password, salt: data.salt,
        memorySize: 65536, iterations: 3, parallelism: 1,
        hashLength: 32, outputType: "binary",
      });
      // Erase inputs before replying. The main thread terminates this dedicated
      // Worker, also releasing hash-wasm's internal WASM memory after every call.
      wipe(data.password);
      wipe(data.salt);
      globalThis.postMessage({ key }, [key.buffer]);
    } catch {
      globalThis.postMessage({ error: "Device-key Argon2id derivation failed." });
    } finally {
      wipe(data?.password);
      wipe(data?.salt);
      wipe(key);
    }
  };
}
