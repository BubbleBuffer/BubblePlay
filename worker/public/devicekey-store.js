// Persist only the .bpkey JSON text, byte-for-byte as created or imported.
// Unlocked CryptoKeys and seed buffers never belong in this database.
const DATABASE = "bubbleplay-device";
const STORE = "keys";
const RECORD = "device";

/** A factory permits isolated IndexedDB tests without browser globals. */
export function createDeviceKeyStore({ indexedDB = globalThis.indexedDB } = {}) {
  function open() {
    return new Promise((resolve, reject) => {
      if (!indexedDB) return reject(new Error("IndexedDB is unavailable for device-key storage."));
      let request, failed = false;
      const fail = error => { failed = true; reject(error); };
      try { request = indexedDB.open(DATABASE, 1); }
      catch (error) { fail(error); return; }
      request.onupgradeneeded = () => {
        const db = request.result;
        if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE);
      };
      request.onerror = () => fail(request.error || new Error("Cannot open device-key database."));
      request.onblocked = () => fail(new Error("Device-key database upgrade is blocked; close other tabs and retry."));
      request.onsuccess = () => {
        const db = request.result;
        // An open request cannot be cancelled after an onblocked rejection.
        if (failed) { db.close(); return; }
        db.onversionchange = () => db.close();
        resolve(db);
      };
    });
  }

  async function transact(mode, operation) {
    const db = await open();
    return new Promise((resolve, reject) => {
      let transaction, request;
      try {
        transaction = db.transaction(STORE, mode);
        request = operation(transaction.objectStore(STORE));
      } catch (error) {
        db.close();
        reject(error);
        return;
      }
      // A request's success precedes commit. Resolve writes only once the whole
      // transaction completes, so quota/commit failures cannot look successful.
      transaction.oncomplete = () => { db.close(); resolve(request.result); };
      transaction.onabort = () => {
        db.close();
        reject(transaction.error || request.error || new Error("Device-key storage transaction aborted."));
      };
    });
  }

  return {
    async load() {
      const text = await transact("readonly", store => store.get(RECORD));
      if (text === undefined) return null;
      if (typeof text !== "string") throw new Error("Stored device key is not JSON text.");
      return text;
    },
    async save(text) {
      if (typeof text !== "string") throw new TypeError("Device-key storage accepts JSON text only.");
      await transact("readwrite", store => store.put(text, RECORD));
    },
    async clear() {
      await transact("readwrite", store => store.delete(RECORD));
    },
  };
}

// Resolve the browser's IndexedDB lazily, also allowing safe import in Node.
export const load = () => createDeviceKeyStore().load();
export const save = text => createDeviceKeyStore().save(text);
export const clear = () => createDeviceKeyStore().clear();
