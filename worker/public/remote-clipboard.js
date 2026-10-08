// Browser clipboard permissions may require a click. Keep the latest host
// snapshot available and serialize writes so an older image cannot win later.
export class RemoteClipboardWriter {
  constructor({ clipboard, ClipboardItem, onPending, autoCopy = true }) {
    this.clipboard = clipboard;
    this.ClipboardItem = ClipboardItem;
    this.onPending = onPending;
    this.pending = null;
    this.latest = null;
    this.active = null;
    this.version = 0;
    this.autoCopy = autoCopy;
    this.focused = false;
    this.requestedVersion = 0;
  }
  clear() { this.version++; this.pending = null; this.latest = null; this.onPending(false); }
  discardPending() {
    if (this.pending !== null) this.latest = null;
    this.version++; this.pending = null; this.onPending(false);
  }
  setAutomatic(enabled, focused) { this.autoCopy = enabled; this.writeAutomatically(focused); }
  writeAutomatically(focused) {
    this.focused = focused;
    if (this.autoCopy && focused) this.write(true);
  }
  receive(content, focused) {
    this.version++;
    this.pending = content;
    this.latest = content;
    this.onPending(true);
    this.writeAutomatically(focused);
  }
  write(automatic = false) {
    if (this.pending === null) return;
    if (!automatic) this.requestedVersion = this.version;
    if (this.active) return;
    const content = this.pending, version = this.version;
    let operation;
    try {
      if (typeof content === "string") {
        if (!this.clipboard?.writeText) return;
        operation = this.clipboard.writeText(content);
      } else {
        if (!this.clipboard?.write || !this.ClipboardItem) return;
        const item = new this.ClipboardItem({ "image/png": new Blob([content.data], { type: "image/png" }) });
        operation = this.clipboard.write([item]);
      }
    } catch { return; }
    this.active = Promise.resolve(operation).then(() => {
      if (this.version === version) { this.pending = null; this.onPending(false); }
    }, () => {}).finally(() => {
      this.active = null;
      if (this.pending !== null && this.version !== version &&
          ((this.autoCopy && this.focused) || this.requestedVersion === this.version)) this.write(true);
    });
  }
}

export function pasteHostClipboard(input, held = new Set()) {
  const modifiers = ["ControlLeft", "ControlRight", "MetaLeft", "MetaRight", "ShiftLeft", "ShiftRight", "AltLeft", "AltRight"].filter(code => held.has(code));
  for (const code of modifiers) input.physicalKey(code, true);
  input.physicalKey("ControlLeft", false);
  input.physicalKey("KeyV", false);
  input.physicalKey("KeyV", true);
  input.physicalKey("ControlLeft", true);
  for (const code of modifiers) input.physicalKey(code, false);
}

async function pixelDigest(blob) {
  const image = await createImageBitmap(blob);
  try {
    if (!image.width || !image.height || image.width * image.height > 16 * 1024 * 1024) return null;
    const canvas = new OffscreenCanvas(image.width, image.height);
    const context = canvas.getContext("2d", { willReadFrequently: true });
    context.drawImage(image, 0, 0);
    const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", context.getImageData(0, 0, image.width, image.height).data));
    return `${image.width}x${image.height}:${Array.from(hash).join(",")}`;
  } finally { image.close(); }
}

export async function sameClipboardImage(content, file, digest = pixelDigest) {
  if (content?.mimeType !== "image/png" || !file?.type?.startsWith("image/") || file.size > 8 * 1024 * 1024) return false;
  try {
    const bytes = new Uint8Array(await file.arrayBuffer());
    if (bytes.length === content.data.length && bytes.every((byte, index) => byte === content.data[index])) return true;
    const [expected, actual] = await Promise.all([digest(new Blob([content.data], { type: "image/png" })), digest(file)]);
    return expected !== null && expected === actual;
  } catch { return false; }
}
