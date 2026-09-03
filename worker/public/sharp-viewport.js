const clamp = (value, min, max) => Math.max(min, Math.min(max, value));
const roundUp = (value, quantum) => Math.ceil(value / quantum) * quantum;
const roundDown = (value, quantum) => Math.floor(value / quantum) * quantum;

export const SHARP_VIEW = Object.freeze({
  enterZoom: 1.10,
  exitZoom: 1.03,
  minAreaReduction: 0.05,
  quantizePx: 16,
  edgeMarginPx: 32,
  minPaddingPx: 48,
  maxPaddingPx: 128,
  dimensionChangeRatio: 0.08,
});

/** Map the visible, transformed media content back into desktop pixels. */
export function visibleDesktopRect({ zoom, x, y, viewportWidth, viewportHeight, content, desktopWidth, desktopHeight }) {
  const z = Math.max(1, Number(zoom) || 1);
  const transformed = {
    left: x + content.x * z,
    top: y + content.y * z,
    right: x + (content.x + content.width) * z,
    bottom: y + (content.y + content.height) * z,
  };
  const left = clamp(Math.max(0, transformed.left), 0, viewportWidth);
  const top = clamp(Math.max(0, transformed.top), 0, viewportHeight);
  const right = clamp(Math.min(viewportWidth, transformed.right), left, viewportWidth);
  const bottom = clamp(Math.min(viewportHeight, transformed.bottom), top, viewportHeight);
  const toDesktopX = screenX => (((screenX - x) / z) - content.x) / Math.max(1, content.width) * desktopWidth;
  const toDesktopY = screenY => (((screenY - y) / z) - content.y) / Math.max(1, content.height) * desktopHeight;
  const sourceLeft = clamp(toDesktopX(left), 0, desktopWidth);
  const sourceTop = clamp(toDesktopY(top), 0, desktopHeight);
  const sourceRight = clamp(toDesktopX(right), sourceLeft, desktopWidth);
  const sourceBottom = clamp(toDesktopY(bottom), sourceTop, desktopHeight);
  return {
    x: sourceLeft,
    y: sourceTop,
    width: Math.max(1, sourceRight - sourceLeft),
    height: Math.max(1, sourceBottom - sourceTop),
  };
}

function paddedAxis(start, size, extent, quantum) {
  const padding = clamp(size * 0.10, SHARP_VIEW.minPaddingPx, SHARP_VIEW.maxPaddingPx);
  const desiredSize = Math.min(extent, roundUp(size + padding * 2, quantum));
  let desiredStart = roundDown(start - (desiredSize - size) / 2, quantum);
  desiredStart = clamp(desiredStart, 0, Math.max(0, extent - desiredSize));
  desiredStart = roundDown(desiredStart, quantum);
  return { start: desiredStart, size: Math.min(desiredSize, extent - desiredStart) };
}

/** Add bounded overscan so camera motion usually remains inside the active crop. */
export function paddedDesktopCrop(visible, desktopWidth, desktopHeight, quantum = SHARP_VIEW.quantizePx) {
  const horizontal = paddedAxis(visible.x, visible.width, desktopWidth, quantum);
  const vertical = paddedAxis(visible.y, visible.height, desktopHeight, quantum);
  return { x: horizontal.start, y: vertical.start, width: horizontal.size, height: vertical.size };
}

export function fullDesktopCrop(desktopWidth, desktopHeight) {
  return { enabled: false, x: 0, y: 0, width: desktopWidth, height: desktopHeight };
}

export function desiredSharpCrop({ viewport, viewportWidth, viewportHeight, content, desktopWidth, desktopHeight, wasEnabled = false }) {
  const snapshot = viewport;
  const threshold = wasEnabled ? SHARP_VIEW.exitZoom : SHARP_VIEW.enterZoom;
  if (snapshot.zoom < threshold) return fullDesktopCrop(desktopWidth, desktopHeight);
  const visible = visibleDesktopRect({ ...snapshot, viewportWidth, viewportHeight, content, desktopWidth, desktopHeight });
  const crop = paddedDesktopCrop(visible, desktopWidth, desktopHeight);
  const reduction = 1 - (crop.width * crop.height) / (desktopWidth * desktopHeight);
  if (reduction < SHARP_VIEW.minAreaReduction) return fullDesktopCrop(desktopWidth, desktopHeight);
  return { enabled: true, ...crop, visible };
}

export function sameCrop(a, b) {
  return !!a && !!b && a.enabled === b.enabled && a.x === b.x && a.y === b.y &&
    a.width === b.width && a.height === b.height;
}

export function cropNeedsUpdate(active, desired, desktopWidth, desktopHeight) {
  if (!active || active.enabled !== desired.enabled) return true;
  if (!desired.enabled) return false;
  const dimensionChanged = Math.abs(active.width - desired.width) / Math.max(1, active.width) >= SHARP_VIEW.dimensionChangeRatio ||
    Math.abs(active.height - desired.height) / Math.max(1, active.height) >= SHARP_VIEW.dimensionChangeRatio;
  if (dimensionChanged) return true;
  const visible = desired.visible;
  if (!visible) return !sameCrop(active, desired);
  const leftSafe = active.x === 0 ? 0 : active.x + SHARP_VIEW.edgeMarginPx;
  const topSafe = active.y === 0 ? 0 : active.y + SHARP_VIEW.edgeMarginPx;
  const rightEdge = active.x + active.width;
  const bottomEdge = active.y + active.height;
  const rightSafe = rightEdge >= desktopWidth ? desktopWidth : rightEdge - SHARP_VIEW.edgeMarginPx;
  const bottomSafe = bottomEdge >= desktopHeight ? desktopHeight : bottomEdge - SHARP_VIEW.edgeMarginPx;
  return visible.x < leftSafe || visible.y < topSafe ||
    visible.x + visible.width > rightSafe || visible.y + visible.height > bottomSafe;
}

export function cropGain(crop, encodedWidth, encodedHeight) {
  if (!crop?.enabled) return { x: 1, y: 1 };
  return { x: encodedWidth / Math.max(1, crop.width), y: encodedHeight / Math.max(1, crop.height) };
}
