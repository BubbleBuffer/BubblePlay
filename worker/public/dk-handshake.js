// Viewer side of the phase-2 handshake. Socket lifetime and timeouts belong to the caller.
import { b64u, buildAuthHello, createCtlChannel, deriveCtlKeys, newViewerHello, signAuthFinish, verifyAuthHost } from "./dk-crypto.js";

export async function beginViewerHandshake({ identity, pcId, pinnedPublic, relayOrigin, epoch, pairing = false, send, helloOptions = {} }) {
  if (!pairing && (!(pinnedPublic instanceof Uint8Array) || pinnedPublic.length !== 32)) {
    throw Object.assign(new Error("normal sessions require a pinned PC key"), { reason: "pc-key-changed" });
  }
  const caps = ["control-aead-v1", "dtls-bind-v1", ...(pairing ? ["pair-v1"] : [])];
  const hello = await newViewerHello({ ...helloOptions, caps });
  send(buildAuthHello({ id: identity.id, publicRaw: identity.publicRaw, nonce: hello.nonce, ephemeralPublic: hello.ephemeralPublic, caps }));
  let consumed = false;
  return {
    async handleAuthHost(msg) {
      if (consumed) throw Object.assign(new Error("auth-host already received"), { reason: "bad-shape" });
      consumed = true;
      if (msg?.id !== pcId) throw Object.assign(new Error("unexpected PC ID"), { reason: "pc-key-changed" });
      if (pinnedPublic && msg.public !== b64u.encode(pinnedPublic)) {
        throw Object.assign(new Error("unexpected PC key"), { reason: "pc-key-changed" });
      }
      const result = await verifyAuthHost({ msg, pinnedPublic: pinnedPublic ?? null, relayOrigin, epoch,
        viewerId: identity.id, viewerPublic: identity.publicRaw, viewerNonce: hello.nonce, viewerEphemeral: hello.ephemeralPublic });
      if (!result.ok) throw Object.assign(new Error("auth-host refused"), { reason: result.reason });
      const keys = await deriveCtlKeys(hello.ephemeralKeyPair.privateKey, b64u.decode(msg.ephemeral), result.H);
      const signature = await signAuthFinish(identity.privateKey, result.H, msg.signature);
      send({ type: "auth-finish", signature });
      return { H: result.H, pcPublic: result.pcPublic, ctl: createCtlChannel({ sendKey: keys.send, recvKey: keys.recv }) };
    },
  };
}
