// Minimal file-transfer UI for the session page: drop preview, offer chip and
// a transfer list with progress and cancel. All state lives in
// FileTransferClient (file-client.js); this only renders it. Hidden unless
// the host advertises `fileTransfer`.
import { formatBytes, validDropPoint } from "./file-transfer.js";
import { chooseDownloadTarget, describeFileError } from "./file-client.js";

const FINISHED_ROW_MS = 8_000;
export const LARGE_DROP_BYTES = 10 * 1024 * 1024;

export function installFileTransferUi({ client, root, veil, resolveDrop = () => ({ purpose: "save" }), onDragPreview = () => {}, onDragPreviewEnd = () => {}, onError = () => {} }) {
  let enabled = false;
  let dragDepth = 0;
  let dragCancelled = false;
  let offerDismissed = null;
  let offerError = null;
  const offerChip = root.querySelector("[data-file-offer]");
  const offerLabel = root.querySelector("[data-file-offer-label]");
  const offerDownload = root.querySelector("[data-file-offer-download]");
  const offerDismiss = root.querySelector("[data-file-offer-dismiss]");
  const list = root.querySelector("[data-file-list]");
  const sweepTimers = new Map();
  const badge = veil.querySelector("div");
  badge.classList.add("file-drop-badge");
  const marker = document.createElement("i");
  marker.className = "file-drop-marker";
  marker.setAttribute("aria-hidden", "true");
  marker.hidden = true;
  veil.append(marker);
  // Separate from the hover veil: protected drag data never creates a transfer.
  const progress = document.createElement("section");
  progress.id = "file-drop-progress";
  progress.hidden = true;
  const progressTitle = document.createElement("strong");
  progressTitle.setAttribute("role", "status");
  const progressDetail = document.createElement("span");
  const progressAction = document.createElement("button");
  progressAction.type = "button";
  progress.append(progressTitle, progressDetail, progressAction);
  // These controls belong to the browser, including Space/Enter activation.
  for (const type of ["keydown", "keyup", "mousedown", "mouseup"]) progress.addEventListener(type, event => event.stopPropagation());
  document.body.append(progress);
  let pendingDrop = null, progressTimer = null;
  function clearDropProgress() {
    pendingDrop = null;
    if (progressTimer !== null) clearInterval(progressTimer);
    progressTimer = null;
    progress.hidden = true;
  }
  function renderDropProgress() {
    if (!pendingDrop) return;
    const rows = client.transfers.filter(row => pendingDrop.ids.includes(row.id));
    if (!enabled || rows.length !== pendingDrop.ids.length || rows.some(row => row.error === "disconnected")) { clearDropProgress(); return; }
    const current = resolveDrop(pendingDrop.event);
    if (current.error || current.purpose !== "drop" || !sameDropPoint(current.drop, pendingDrop.drop)) { clearDropProgress(); return; }
    const state = largeDropStatus(rows);
    progress.dataset.state = state.state;
    // Avoid re-announcing an unchanged live status on geometry heartbeats.
    if (progressTitle.textContent !== state.title) progressTitle.textContent = state.title;
    if (progressDetail.textContent !== state.detail) progressDetail.textContent = state.detail;
    progressAction.textContent = state.terminal ? "Dismiss" : "Cancel";
    progressAction.setAttribute("aria-label", state.terminal ? "Dismiss file drop status" : "Cancel this file drop");
    progress.hidden = !veil.hidden;
    if (!progress.hidden) {
      const position = positionFileDropBadge(pendingDrop.point, progress.getBoundingClientRect(), { width: window.innerWidth, height: window.innerHeight });
      if (!position) { clearDropProgress(); return; }
      progress.style.left = `${position.x}px`;
      progress.style.top = `${position.y}px`;
    }
    if (state.terminal) {
      pendingDrop.finishedAt ??= Date.now();
      if (Date.now() - pendingDrop.finishedAt >= FINISHED_ROW_MS) clearDropProgress();
    }
  }
  progressAction.onclick = event => {
    event.stopPropagation();
    if (!pendingDrop) return;
    const rows = client.transfers.filter(row => pendingDrop.ids.includes(row.id));
    if (largeDropStatus(rows).terminal) clearDropProgress();
    else {
      const active = rows.find(row => !["done", "failed", "cancelled"].includes(row.state));
      if (active) client.cancel(active.id); // The client cancels this native batch.
    }
  };

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
    list.replaceChildren(...(offerError ? [renderOfferError()] : []), ...rows.map(renderRow));
    root.hidden = !enabled || (!showOffer && rows.length === 0 && !offerError);
    renderDropProgress();
    for (const row of rows) {
      if (row.finishedAt && row.state === "done" && !sweepTimers.has(row.id)) {
        sweepTimers.set(row.id, setTimeout(() => { sweepTimers.delete(row.id); client.dismiss(row.id); }, FINISHED_ROW_MS));
      }
    }
  }

  function renderOfferError() {
    const item = document.createElement("li");
    item.className = "file-row";
    item.dataset.state = "failed";
    const status = document.createElement("span");
    status.className = "file-row-status";
    status.setAttribute("role", "status");
    status.textContent = offerError;
    const dismiss = document.createElement("button");
    dismiss.type = "button";
    dismiss.textContent = "Dismiss";
    dismiss.onclick = event => { event.stopPropagation(); offerError = null; render(); };
    item.append(status, dismiss);
    return item;
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

  offerDownload.onclick = async event => {
    event.stopPropagation();
    const offer = client.offer;
    if (!offer) return;
    offerError = null;
    render();
    let sinkFor;
    try {
      sinkFor = await chooseDownloadTarget(offer.items);
    } catch (error) {
      onError(error);
      return;
    }
    if (!sinkFor) return;
    try {
      client.download(offer.items, sinkFor, offer.offerId);
    } catch (error) {
      if (error?.code === "offer-replaced") { offerError = describeFileError(error.code); render(); }
      onError(error);
      return;
    }
    offerDismissed = offer.offerId;
    render();
  };
  offerDismiss.onclick = event => { event.stopPropagation(); offerDismissed = client.offer?.offerId ?? null; render(); };

  // Drop anywhere on the page (the input surface covers the stream).
  const hasFiles = event => Array.from(event.dataTransfer?.types || []).includes("Files");
  function hideDropHint() {
    dragDepth = 0;
    veil.hidden = true;
    marker.hidden = true;
    onDragPreviewEnd();
    renderDropProgress();
  }
  function showDropHint(event) {
    // Count item kinds only. Files, names and directory entries remain unread
    // until the browser makes them available at the actual drop.
    const target = resolveDrop(event);
    const title = veil.querySelector("strong");
    const detail = veil.querySelector("span");
    const preview = target.error || target.purpose === "drop";
    veil.dataset.mode = preview ? "preview" : "save";
    veil.dataset.state = target.error ? "blocked" : "ready";
    veil.hidden = false;
    badge.hidden = false;
    if (target.error) {
      title.textContent = "Can't drop here yet";
      detail.textContent = describeFileError(target.error);
    } else if (target.purpose === "drop") {
      const count = Array.from(event.dataTransfer?.items || []).filter(item => item.kind === "file").length;
      title.textContent = "Drop on PC here";
      detail.textContent = `${count ? `${count} ${count === 1 ? "file" : "files"} · ` : ""}Release to send to this spot.`;
    } else {
      title.textContent = "Drop to send to the PC";
      detail.textContent = "Saved in Downloads\\BubblePlay and put on the PC clipboard.";
    }
    marker.hidden = !preview;
    if (preview) {
      const point = { x: event.clientX, y: event.clientY };
      if (![point.x, point.y].every(Number.isFinite)) { hideDropHint(); return; }
      marker.style.left = `${point.x}px`;
      marker.style.top = `${point.y}px`;
      const position = positionFileDropBadge(point, badge.getBoundingClientRect(), { width: window.innerWidth, height: window.innerHeight });
      badge.hidden = !position;
      if (position) { badge.style.left = `${position.x}px`; badge.style.top = `${position.y}px`; }
    }
    event.dataTransfer.dropEffect = target.error ? "none" : "copy";
    onDragPreview(event, target);
    renderDropProgress();
  }
  window.addEventListener("dragenter", event => {
    if (!enabled || !hasFiles(event)) return;
    event.preventDefault();
    dragCancelled = false;
    dragDepth += 1;
    showDropHint(event);
  });
  window.addEventListener("dragover", event => {
    if (!enabled || dragCancelled || !hasFiles(event)) return;
    event.preventDefault();
    showDropHint(event);
  });
  window.addEventListener("dragleave", event => {
    if (veil.hidden) return;
    // Protected dragleave data may have no types, including a window exit.
    const leftWindow = event.relatedTarget == null && (event.clientX <= 0 || event.clientY <= 0 || event.clientX >= window.innerWidth || event.clientY >= window.innerHeight);
    if (leftWindow) { hideDropHint(); return; }
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) hideDropHint();
  });
  window.addEventListener("drop", event => {
    if (!hasFiles(event)) return;
    event.preventDefault();
    hideDropHint();
    if (!enabled || dragCancelled) return;
    // v1 sends files only: skip dropped folders.
    const files = [];
    for (const item of Array.from(event.dataTransfer.items || [])) {
      if (item.kind !== "file" || item.webkitGetAsEntry?.()?.isDirectory) continue;
      const file = item.getAsFile();
      if (file) files.push(file);
    }
    if (files.length) {
      // Capture the point from this drop event, never the later remote cursor.
      const target = resolveDrop(event);
      if (target.error) client.rejectUpload(files, "drop", target.error);
      else {
        const ids = client.upload(files, target.purpose, target.drop);
        if (target.purpose === "drop" && files.reduce((sum, file) => sum + file.size, 0) >= LARGE_DROP_BYTES && ids?.length) {
          clearDropProgress();
          pendingDrop = { ids: [...ids], drop: { ...target.drop }, point: { x: event.clientX, y: event.clientY },
            event: { clientX: event.clientX, clientY: event.clientY, target: event.target } };
          renderDropProgress();
          if (pendingDrop) progressTimer = setInterval(renderDropProgress, 500);
        }
      }
    }
  });
  window.addEventListener("blur", hideDropHint);
  window.addEventListener("dragend", hideDropHint);
  window.addEventListener("resize", () => { hideDropHint(); clearDropProgress(); });
  window.addEventListener("keydown", event => { if (event.key === "Escape") { dragCancelled = true; hideDropHint(); } });
  document.addEventListener("visibilitychange", () => { if (document.hidden) hideDropHint(); });

  client.addEventListener("change", render);
  render();

  return {
    setEnabled(value) {
      enabled = Boolean(value);
      if (!enabled) { offerError = null; hideDropHint(); clearDropProgress(); }
      render();
    },
    get enabled() { return enabled; },
  };
}

function sameDropPoint(a, b) {
  return a && b && ["x", "y", "generation", "viewportRevision"].every(key => a[key] === b[key]);
}

/** ACKed bytes describe transfer progress; only terminal host results mean success. */
export function largeDropStatus(rows) {
  const total = rows.reduce((sum, row) => sum + row.size, 0);
  const bytes = rows.reduce((sum, row) => sum + Math.max(0, Math.min(row.size, row.bytes || 0)), 0);
  const failed = rows.find(row => row.state === "failed");
  const terminal = rows.every(row => ["done", "failed", "cancelled"].includes(row.state));
  const count = `${rows.length} ${rows.length === 1 ? "file" : "files"}`;
  if (failed) return { state: "failed", terminal, title: "Couldn't drop files on PC", detail: describeFileError(failed.error) };
  if (rows.some(row => row.state === "cancelled")) return { state: "cancelled", terminal, title: "File drop cancelled", detail: "Check the PC app before trying again." };
  if (terminal) return { state: "done", terminal, title: "Dropped on PC", detail: `${count} · ${formatBytes(total)}` };
  if (bytes === total && rows.every(row => ["verifying", "done"].includes(row.state))) return { state: "attaching", terminal, title: "Attaching…", detail: "Waiting for the PC app to accept." };
  return { state: "sending", terminal, title: `Sending files to PC… ${total ? Math.min(100, Math.floor(bytes / total * 100)) : 0}%`, detail: `${count} · ${formatBytes(bytes)} of ${formatBytes(total)}` };
}

/** Metadata-only signaling for a host hover that cannot itself drop files. */
export function createFileDragPreviewController({ send, getContext, resolveDrop, now = () => performance.now(), every = setInterval, stop = clearInterval }) {
  let active = null, timer = null, nextId = 0, lastSent = -Infinity;
  const utf8 = new TextEncoder();
  const safeSend = message => { try { return send(message) !== false; } catch { return false; } };
  function cancel() {
    if (timer !== null) stop(timer);
    timer = null;
    const previous = active;
    active = null;
    if (previous && getContext().epoch === previous.epoch) safeSend({ type: "file-drag-preview", protocol: 1, epoch: previous.epoch, dragId: previous.id, action: "cancel" });
  }
  function publish(target) {
    const context = getContext();
    if (!active || !context.available || context.epoch !== active.epoch || target?.purpose !== "drop" || !validDropPoint(target.drop)) { cancel(); return; }
    const message = { type: "file-drag-preview", protocol: 1, epoch: active.epoch, dragId: active.id, action: "update", drop: { ...target.drop }, types: [...active.types] };
    // JSON-escaped hints can exceed the raw control-message bound
    // despite individually bounded hints. Retain count with unknown types.
    if (utf8.encode(JSON.stringify(message)).length > 8192) message.types = active.types.map(() => "");
    if (!safeSend(message)) { cancel(); return; }
    lastSent = now();
  }
  return {
    update(event, target = resolveDrop(event)) {
      const context = getContext();
      if (!context.available || !/^[a-f0-9]{32}$/.test(context.epoch || "") || target?.purpose !== "drop" || !validDropPoint(target.drop)) { cancel(); return; }
      if (active && active.epoch !== context.epoch) { cancel(); return; }
      const types = Array.from(event.dataTransfer?.items || []).filter(item => item.kind === "file").slice(0, 64)
        .map(item => typeof item.type === "string" && /^[\x21-\x7e]*$/.test(item.type) && utf8.encode(item.type).length <= 96 ? item.type : "");
      if (!active) {
        nextId = (nextId + 1) >>> 0 || 1;
        active = { id: nextId, epoch: context.epoch, event, types: types.length ? types : [""] };
        lastSent = -Infinity;
        timer = every(() => { if (active) publish(resolveDrop(active.event)); }, 500);
      } else { active.event = event; active.types = types.length ? types : [""]; }
      if (now() - lastSent >= 200) publish(target);
    },
    cancel,
  };
}

/** Keep the native badge off the literal drop point and inside the viewport.
 * At an impossibly small viewport retain only the exact marker.
 */
export function positionFileDropBadge(point, size, viewport) {
  if (![point.x, point.y, size.width, size.height, viewport.width, viewport.height].every(Number.isFinite) ||
      size.width <= 0 || size.height <= 0 || viewport.width < size.width + 16 || viewport.height < size.height + 16) return null;
  const gap = 24, pad = 8;
  const clamp = (value, maximum) => Math.min(Math.max(value, pad), maximum - pad);
  const candidates = [[point.x + gap, point.y + gap], [point.x - size.width - gap, point.y + gap],
    [point.x + gap, point.y - size.height - gap], [point.x - size.width - gap, point.y - size.height - gap]];
  for (const [x, y] of candidates) {
    if (x >= pad && y >= pad && x + size.width <= viewport.width - pad && y + size.height <= viewport.height - pad) return { x, y };
  }
  for (const [x, y] of candidates) {
    const placed = { x: clamp(x, viewport.width - size.width), y: clamp(y, viewport.height - size.height) };
    if (point.x < placed.x - 12 || point.x > placed.x + size.width + 12 || point.y < placed.y - 12 || point.y > placed.y + size.height + 12) return placed;
  }
  return null;
}

export function statusText(row) {
  const upload = row.direction === "upload";
  switch (row.state) {
    case "queued": return upload ? "Waiting to send" : "Waiting to download";
    case "connecting": return "Connecting to the PC…";
    case "active": return `${formatBytes(row.bytes)} of ${formatBytes(row.size)} · ${upload ? "sending to PC" : "downloading"}`;
    case "verifying": return row.purpose === "drop" ? "Waiting for the PC app to accept…" : "Checking the file…";
    case "done":
      if (!upload) return "Downloaded";
      if (row.purpose === "drop") return "Dropped on PC";
      return row.purpose === "paste" ? "Saved and pasted on the PC" : "Saved in Downloads\\BubblePlay · on the PC clipboard";
    case "cancelled": return row.purpose === "drop" ? "File drop cancelled" : "Cancelled; nothing was kept";
    default: return describeFileError(row.error);
  }
}

/** Map an actual rendered content rectangle (including CSS pan/zoom) to q16.
 * Reject bars and clipped UI; callers separately reject stale media geometry.
 */
export function mapFileDropPoint(point, content, clip) {
  const inside = rect => rect && [rect.left, rect.top, rect.width, rect.height].every(Number.isFinite) &&
    rect.width > 0 && rect.height > 0 && point.x >= rect.left && point.y >= rect.top &&
    point.x <= rect.left + rect.width && point.y <= rect.top + rect.height;
  if (!point || !Number.isFinite(point.x) || !Number.isFinite(point.y) || !inside(content) || !inside(clip)) return null;
  return { x: Math.round((point.x - content.left) / content.width * 65535),
    y: Math.round((point.y - content.top) / content.height * 65535) };
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
