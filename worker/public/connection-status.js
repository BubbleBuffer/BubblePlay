// Pure model of the connection chip in the session strip: what state, label and tooltip to show.
// The DOM is applied by the caller; this module reads no globals.

export const CONNECTION_STATE = Object.freeze({ CONNECTING: "connecting", CONNECTED: "connected", ERROR: "error" });
const LABEL_SEPARATOR = " · ";
const DETAIL_SEPARATOR = " via ";
const ICE_CONNECTED_STATES = Object.freeze(["connected", "completed", "connected/connected"]);
const WSS_MEDIA_LABELS = Object.freeze({
  connected: "Connected · WSS",
  "paused-static": "Connected · WSS (static)",
  reconnecting: "Reconnecting media…",
  starting: "Starting stream…",
});
const WSS_CONNECTED_STATES = Object.freeze(["connected", "paused-static"]);
const ADMIN_WARNINGS = Object.freeze({
  "admin-required": "This window needs Admin mode — turn it on at the PC",
  "admin-unavailable": "Admin mode helper isn't responding",
});

function transportState(s) {
  if (s.terminalSignalingFailure) return { state: CONNECTION_STATE.ERROR, label: "Session unavailable" };
  if (!s.desktopAvailable) return { state: CONNECTION_STATE.ERROR, label: "Desktop unavailable" };
  if (s.usingWssMedia) {
    // v4 never reports "waiting for stream" for a static desktop: no frames is the expected
    // steady state while nothing on screen changes.
    const state = WSS_CONNECTED_STATES.includes(s.wssStatus) ? CONNECTION_STATE.CONNECTED : CONNECTION_STATE.CONNECTING;
    return { state, label: WSS_MEDIA_LABELS[s.wssStatus] ?? "Connecting…" };
  }
  if (s.inputReady && ICE_CONNECTED_STATES.includes(s.ice)) return { state: CONNECTION_STATE.CONNECTED, label: "Connected · WebRTC" };
  if (s.ws.startsWith("reconnecting") || s.ws === "closed") return { state: CONNECTION_STATE.CONNECTING, label: "Reconnecting…" };
  if (s.ws === "error") return { state: CONNECTION_STATE.ERROR, label: "Connection error" };
  return { state: CONNECTION_STATE.CONNECTING, label: "Connecting…" };
}

// s: terminalSignalingFailure, desktopAvailable, usingWssMedia, wssStatus, inputReady, ice, ws,
// desktopKind, inputStatusReason, rttMs (number or null), monitorName (string or null).
export function describeConnectionStatus(s) {
  const { state, label } = transportState(s);
  const detailLabel = label.replace(LABEL_SEPARATOR, DETAIL_SEPARATOR);
  const warning = s.desktopAvailable && s.desktopKind === "default" && !s.terminalSignalingFailure
    ? ADMIN_WARNINGS[s.inputStatusReason] ?? "" : "";
  const shortLabel = warning || label.split(LABEL_SEPARATOR)[0];
  const detail = [warning, detailLabel, s.rttMs === null ? null : `round trip ${s.rttMs} ms`, s.monitorName]
    .filter(Boolean).join(LABEL_SEPARATOR);
  return { state, shortLabel, detail, connectionDetail: detailLabel, warning };
}
