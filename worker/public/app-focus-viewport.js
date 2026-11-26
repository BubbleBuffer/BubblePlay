// The app focus crop as a Viewport state (docs/architecture/app-focus-mode.md). The crop
// is the window rectangle as fractions of the streamed monitor; the viewport zooms and pans
// so it fills the stream area and nothing else is shown. Because it is the viewport's own
// zoom and pan, #visual's transform places the picture, the mouse path (videoContentRect
// reads the transformed element) and the touch path (viewport + RemoteCursor) both map
// back through the crop with no second mapping.
import { Viewport } from "./input.js";

const HALF = 0.5;
const clamp = (value, min, max) => Math.max(min, Math.min(max, value));

export class FocusViewport extends Viewport {
  /** { x, y, width, height } in 0..1 of the desktop, or null for the whole desktop. */
  focus = null;
  setFocus(crop) {
    if (!crop && this.focus) { this.zoom = 1; this.x = this.y = 0; } // leaving: back to the whole desktop, unzoomed
    this.focus = crop;
    return this.clamp();
  }
  clamp() {
    const crop = this.focus;
    if (!crop) return super.clamp();
    const c = this.content;
    this.zoom = Math.min(this.width / (crop.width * c.width), this.height / (crop.height * c.height));
    this.x = this.left + this.width * HALF - (c.x + (crop.x + crop.width * HALF) * c.width) * this.zoom;
    this.y = this.top + this.height * HALF - (c.y + (crop.y + crop.height * HALF) * c.height) * this.zoom;
    return this.snapshot();
  }
  /** A desktop point (0..1) kept inside the crop: the letterbox is not part of the app. */
  clampPoint(x, y) {
    const crop = this.focus;
    if (!crop) return { x, y };
    return { x: clamp(x, crop.x, crop.x + crop.width), y: clamp(y, crop.y, crop.y + crop.height) };
  }
  /** clip-path for #visual (its local box, before the transform) that hides all but the crop. */
  clipPath(area) {
    const crop = this.focus;
    if (!crop) return "";
    const c = this.content;
    const left = c.x - area.left + crop.x * c.width, top = c.y - area.top + crop.y * c.height;
    const width = crop.width * c.width, height = crop.height * c.height;
    return `inset(${top}px ${area.width - left - width}px ${area.height - top - height}px ${left}px)`;
  }
}
