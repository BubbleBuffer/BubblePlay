export function requireLabUrl(value) {
  if (typeof value !== "string" || !value.trim()) {
    throw new Error("A relay URL is required: pass --url or set BUBBLEPLAY_URL.");
  }
  let url;
  try { url = new URL(value); } catch {
    throw new Error("The relay URL must be an absolute HTTP or HTTPS URL.");
  }
  if (!["http:", "https:"].includes(url.protocol)) {
    throw new Error("The relay URL must be an absolute HTTP or HTTPS URL.");
  }
  return url.href;
}
