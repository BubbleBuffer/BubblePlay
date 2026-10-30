// Persist only the .bpkey JSON text, byte-for-byte as created or imported.
// Unlocked CryptoKeys and seed buffers never belong in this database.
// The "pcs" store holds the paired PCs; IndexedDB is per origin, so the list is
// per relay origin. Records: {id (canonical), name, public (b64url raw 32 bytes,
// pinned at pairing), paired (ms), lastUsed (ms or null)}.
import { canonicalId } from "./devicekey.js";

const DATABASE = "bubbleplay-device";
const VERSION = 2;
const STORE = "keys";
const PCS = "pcs";
const RECORD = "device";
// The record a format conversion replaces, kept until the new one reads back (dk-app.js). Removed with the key.
const BACKUP = "device-backup";
const PC_NAME_MAX = 80;

/** Trims and caps a PC name at 80 code points; an empty name falls back to the ID. */
export function pcName(name, id) {
  const text = [...String(name ?? "").trim()].slice(0, PC_NAME_MAX).join("").trim();
  return text || id;
}

function validPublic(text) {
  return typeof text === "string" && /^[A-Za-z0-9_-]{43}$/.test(text);
}

function validTime(value, nullable) {
  return (nullable && value === null) || (Number.isSafeInteger(value) && value >= 0);
}

/** Validates and canonicalises a PC record; throws TypeError when it is malformed. */
export function normalizePcRecord(record) {
  const id = canonicalId(record?.id);
  if (!validPublic(record.public)) throw new TypeError("A PC record needs its pinned 32-byte public key.");
  if (!validTime(record.paired, false)) throw new TypeError("A PC record needs its pairing time.");
  const lastUsed = record.lastUsed ?? null;
  if (!validTime(lastUsed, true)) throw new TypeError("A PC record's last use is a time or null.");
  return { id, name: pcName(record.name, id), public: record.public, paired: record.paired, lastUsed };
}

/** A factory permits isolated IndexedDB tests without browser globals. */
export function createDeviceKeyStore({ indexedDB = globalThis.indexedDB } = {}) {
  function open() {
    return new Promise((resolve, reject) => {
      if (!indexedDB) return reject(new Error("IndexedDB is unavailable for device-key storage."));
      let request, failed = false;
      const fail = error => { failed = true; reject(error); };
      try { request = indexedDB.open(DATABASE, VERSION); }
      catch (error) { fail(error); return; }
      request.onupgradeneeded = () => {
        const db = request.result;
        if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE);
        if (!db.objectStoreNames.contains(PCS)) db.createObjectStore(PCS, { keyPath: "id" });
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

  async function transact(mode, operation, name = STORE) {
    const db = await open();
    return new Promise((resolve, reject) => {
      let transaction, request;
      try {
        transaction = db.transaction(name, mode);
        request = operation(transaction.objectStore(name));
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

  // Read-modify-write inside one transaction; resolves the new record, or null when absent.
  function updatePc(id, change) {
    const key = canonicalId(id);
    return transact("readwrite", store => {
      const result = { result: null };
      const read = store.get(key);
      read.onsuccess = () => {
        if (!read.result) return;
        const next = normalizePcRecord(change(read.result));
        store.put(next);
        result.result = next;
      };
      return result;
    }, PCS);
  }

  const pcStore = {
    async listPcs() {
      const records = await transact("readonly", store => store.getAll(), PCS);
      return Array.isArray(records) ? records : [];
    },
    async getPc(id) {
      let key;
      try { key = canonicalId(id); } catch { return null; }
      return (await transact("readonly", store => store.get(key), PCS)) ?? null;
    },
    async putPc(record) {
      const next = normalizePcRecord(record);
      await transact("readwrite", store => store.put(next), PCS);
      return next;
    },
    renamePc: (id, name) => updatePc(id, record => ({ ...record, name: pcName(name, record.id) })),
    touchPc: (id, now = Date.now()) => updatePc(id, record => ({ ...record, lastUsed: now })),
    async removePc(id) {
      const key = canonicalId(id);
      await transact("readwrite", store => store.delete(key), PCS);
    },
  };

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
    async loadBackup() {
      const text = await transact("readonly", store => store.get(BACKUP));
      if (text === undefined) return null;
      if (typeof text !== "string") throw new Error("Stored device-key backup is not JSON text.");
      return text;
    },
    async saveBackup(text) {
      if (typeof text !== "string") throw new TypeError("Device-key storage accepts JSON text only.");
      await transact("readwrite", store => store.put(text, BACKUP));
    },
    async clearBackup() {
      await transact("readwrite", store => store.delete(BACKUP));
    },
    async clear() {
      await transact("readwrite", store => store.delete(RECORD));
      await transact("readwrite", store => store.delete(BACKUP));
    },
    ...pcStore,
  };
}

// Resolve the browser's IndexedDB lazily, also allowing safe import in Node.
export const load = () => createDeviceKeyStore().load();
export const save = text => createDeviceKeyStore().save(text);
export const clear = () => createDeviceKeyStore().clear();
export const touchPc = id => createDeviceKeyStore().touchPc(id);
