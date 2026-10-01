// Minimal file-transfer UI for the session page: drop veil, offer chip and
// a transfer list with progress and cancel. All state lives in
// FileTransferClient (file-client.js); this only renders it. Hidden unless
// the host advertises `fileTransfer`.
import { formatBytes } from "./file-transfer.js";
import { chooseDownloadTarget, describeFileError } from "./file-client.js";

const FINISHED_ROW_MS = 8_000;

export function installFileTransferUi({ client, root, veil, onError = () => {} }) {
  let enabled = false;
  let dragDepth = 0;
  let offerDismissed = null;
  const offerChip = root.querySelector("[data-file-offer]");
  const offerLabel = root.querySelector("[data-file-offer-label]");
  const offerDownload = root.querySelector("[data-file-offer-download]");
  const offerDismiss = root.querySelector("[data-file-offer-dismiss]");
  const list = root.querySelector("[data-file-list]");
  const sweepTimers = new Map();

  function render() {
    const offer = client.offer;
    const showOffer = enabled && offer && offerDismissed !== offer.offerId;
    offerChip.hidden = !showOffer;
    if (showOffer) {
      const count = offer.items.length;
      const total = offer.items.reduce((sum, item) => sum + item.size, 0);
      offerLabel.textContent = `${count} ${count === 1 ? "file" : "files"} from PC · ${formatBytes(total)}`;
      offerChip.title = offer.items.map(item => item.name).join("\n");
    }
    const rows = enabled ? client.transfers : [];
    list.replaceChildren(...rows.map(renderRow));
    root.hidden = !enabled || (!showOffer && rows.length === 0);
    for (const row of rows) {
      if (row.finishedAt && row.state === "done" && !sweepTimers.has(row.id)) {
        sweepTimers.set(row.id, setTimeout(() => { sweepTimers.delete(row.id); client.dismiss(row.id); }, FINISHED_ROW_MS));
      }
    }
  }

  function renderRow(row) {
    const item = document.createElement("li");
    item.className = "file-row";
    item.dataset.state = row.state;
    item.dataset.direction = row.direction;
    const name = document.createElement("span");
    name.className = "file-row-name";
    name.textContent = row.name;
    name.title = row.name;
    const status = document.createElement("span");
    status.className = "file-row-status";
    status.textContent = statusText(row);
    const bar = document.createElement("span");
    bar.className = "file-row-bar";
    bar.setAttribute("role", "progressbar");
    bar.setAttribute("aria-label", `${row.name} progress`);
    bar.setAttribute("aria-valuemin", "0");
    bar.setAttribute("aria-valuemax", "100");
    const percent = row.size > 0 ? Math.min(100, Math.floor((row.bytes / row.size) * 100)) : (row.state === "done" ? 100 : 0);
    bar.setAttribute("aria-valuenow", String(percent));
    const fill = document.createElement("span");
    fill.style.width = `${percent}%`;
    bar.append(fill);
    const action = document.createElement("button");
    action.type = "button";
    const terminal = ["done", "failed", "cancelled"].includes(row.state);
    action.textContent = terminal ? "Dismiss" : "Cancel";
    action.setAttribute("aria-label", `${terminal ? "Dismiss" : "Cancel"} ${row.name}`);
    action.onclick = event => { event.stopPropagation(); terminal ? client.dismiss(row.id) : client.cancel(row.id); };
    item.append(name, action, bar, status);
    return item;
  }

  function statusText(row) {
    const upload = row.direction === "upload";
    switch (row.state) {
      case "queued": return upload ? "Waiting to send" : "Waiting to download";
      case "connecting": return "Connecting to the PC…";
      case "active": return `${formatBytes(row.bytes)} of ${formatBytes(row.size)} · ${upload ? "sending to PC" : "downloading"}`;
      case "verifying": return "Checking the file…";
      case "done":
        if (!upload) return "Downloaded";
        return row.purpose === "paste" ? "Saved and pasted on the PC" : "Saved in Downloads\\BubblePlay · on the PC clipboard";
      case "cancelled": return "Cancelled; nothing was kept";
      default: return describeFileError(row.error);
    }
  }

  offerDownload.onclick = async event => {
    event.stopPropagation();
    const offer = client.offer;
    if (!offer) return;
    let sinkFor;
    try {
      sinkFor = await chooseDownloadTarget(offer.items);
    } catch (error) {
      onError(error);
      return;
    }
    if (!sinkFor) return;
    offerDismissed = offer.offerId;
    client.download(offer.items, sinkFor);
  };
  offerDismiss.onclick = event => { event.stopPropagation(); offerDismissed = client.offer?.offerId ?? null; render(); };

  // Drop anywhere on the page (the input surface covers the stream).
  const hasFiles = event => Array.from(event.dataTransfer?.types || []).includes("Files");
  window.addEventListener("dragenter", event => {
    if (!enabled || !hasFiles(event)) return;
    event.preventDefault();
    dragDepth += 1;
    veil.hidden = false;
  });
  window.addEventListener("dragover", event => {
    if (!enabled || !hasFiles(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "copy";
  });
  window.addEventListener("dragleave", event => {
    if (!enabled || !hasFiles(event)) return;
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) veil.hidden = true;
  });
  window.addEventListener("drop", event => {
    if (!hasFiles(event)) return;
    event.preventDefault();
    dragDepth = 0;
    veil.hidden = true;
    if (!enabled) return;
    // v1 sends files only: skip dropped folders.
    const files = [];
    for (const item of Array.from(event.dataTransfer.items || [])) {
      if (item.kind !== "file" || item.webkitGetAsEntry?.()?.isDirectory) continue;
      const file = item.getAsFile();
      if (file) files.push(file);
    }
    if (files.length) client.upload(files, "save");
  });

  client.addEventListener("change", render);
  render();

  return {
    setEnabled(value) {
      enabled = Boolean(value);
      if (!enabled) veil.hidden = true;
      render();
    },
    get enabled() { return enabled; },
  };
}

/** Files (or a pasted image) carried by a paste event, renamed when the browser gives a generic name. */
export function filesFromPaste(event) {
  const files = Array.from(event.clipboardData?.files || []);
  if (!files.length) return [];
  const stamp = new Date().toISOString().replace("T", " ").slice(0, 19).replaceAll(":", "-");
  return files.map(file => /^image\.(png|jpe?g|gif|webp)$/i.test(file.name)
    ? new File([file], `Pasted image ${stamp}.${file.name.split(".").pop()}`, { type: file.type, lastModified: file.lastModified })
    : file);
}
