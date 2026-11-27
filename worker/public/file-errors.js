// User-facing text for file-transfer error codes (see file-client.js).

/** Short, user-facing text for protocol error codes. */
export function describeFileError(code) {
  switch (code) {
    case "hash-mismatch": return "The file arrived damaged; nothing was kept.";
    case "size-mismatch": return "The file size changed during the transfer; nothing was kept.";
    case "seq-gap": case "unexpected": return "The transfer got out of step; nothing was kept.";
    case "too-large": return "Larger than 2 GB, the limit per file.";
    case "busy": return "The PC is busy with another file.";
    case "write-failed": return "The PC could not save the file.";
    case "unreadable": return "The PC could not read the file.";
    case "unknown-offer": return "The PC no longer offers this file.";
    case "unannounced": case "duplicate-batch": return "The PC did not accept this transfer. Try again.";
    case "unauthorized": return "The PC did not authorize this download. Try again.";
    case "offer-replaced": return "The PC replaced these files. Try again.";
    case "window-exceeded": return "The PC sent more file data than the browser could save; the transfer was stopped.";
    case "timeout": return "The transfer stalled.";
    case "no-peer": case "not-ready": return "The PC is not ready for files. Try again in a moment.";
    case "disconnected": return "The connection dropped; the transfer was stopped.";
    case "too-many": return "More than 64 files in one go.";
    case "blob-too-large": return "Over 256 MB: this browser can only save it with Chrome or Edge on a computer.";
    case "save-failed": return "The browser could not save the file.";
    case "invalid-drop": return "The file drop could not be prepared. Try again.";
    case "drop-not-ready": return "Wait for the PC stream to be ready, then drop again.";
    case "drop-stale-view": return "The PC view changed. Drop the files again at the new spot.";
    case "drop-desktop-unavailable": return "Wait for the PC desktop to return, then drop again.";
    case "drop-timeout": return "The PC app took too long to accept the files. Try again.";
    case "drop-input-busy": return "Release the PC's keys and mouse buttons, then drop again.";
    case "drop-target-moved": case "drop-target-changed": return "The drop target changed. Drop the files again.";
    case "drop-interrupted": return "PC input changed while sending. Drop the files again.";
    case "drop-rejected": return "The PC app did not accept these files. Try another drop area.";
    case "drop-failed": return "The PC could not complete the file drop.";
    case "drop-unavailable": return "File dropping is unavailable on the PC right now.";
    case "drop-cancelled": return "The file drop was cancelled.";
    case "drop-too-large": return "The files exceed the 2 GB limit for one drop.";
    default: return code ? `Stopped (${code}).` : "Stopped.";
  }
}
