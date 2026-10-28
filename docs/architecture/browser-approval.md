# Browser approval

Status: approved 2026-10-06 (owner); Go methods, host/ntfy approval, methods endpoint and operator documentation implemented; live host/container acceptance remains unverified.

This page describes the Go and Cloudflare relays. The Rust relay's browser viewer does not use it: it needs only the ntfy site gate, then a device key ([browser viewer](../browser-viewer.md)).

On the Go and Cloudflare relays, a new browser needs the application password plus one second factor. The operator chooses which second factors a relay offers. There is no password-only mode.

## Methods

| Method | Go relay | Cloudflare relay | How it works |
| --- | --- | --- | --- |
| `totp` | yes | no | Six-digit authenticator code entered with the password. |
| `ntfy` | yes | yes | The relay publishes Approve/Deny links to an ntfy topic. |
| `host` | yes | yes | The connected Windows host shows an Approve/Deny dialog on the PC. |

`ntfy` and `host` are approval notifiers. When a browser requests approval, the relay sends the request to every enabled notifier at once; the first decision wins and the others are cancelled. An approved browser receives the gate cookie and then signs in with the password.

## Configuration

- Go: `approvalMethods` in `config.json`, a non-empty subset of `totp`, `ntfy`, `host`. `relay init --methods totp,host` sets it; `relay rotate --methods ...` changes it. A config without the field keeps its old behaviour (`totp` when a TOTP secret exists, `ntfy` when ntfy is configured).
- Cloudflare: `APPROVAL_METHODS` var, a comma list of `ntfy`, `host`. Unset means `ntfy`.
- A relay refuses to start when the list is empty, names an unknown method, or names `ntfy` without its URL, topic and token.

## Login page

- With `totp` enabled, the page asks for password and code, and offers "Request approval instead" when a notifier is enabled.
- Without `totp`, opening the site starts the approval request directly, as the ntfy flow does today.
- `GET /__auth/methods` (unauthenticated) returns `{"totp": bool, "approval": ["ntfy", "host"], "hostOnline": bool}` so the page can say when host approval cannot reach the PC.

## Host protocol

Carried on the existing `/host-ws` signaling socket.

- The host announces `"browserApproval": true` in `host-capabilities`. A relay sends host approval requests only to a host that announced it.
- The per-user tray is the supported runtime for host approval; Session 0, legacy service transport and hosts without an interactive window station and desktop do not advertise it.
- Relay to host: `{"type":"approval-request","id":"<opaque, 22-64 chars>","client":"<IP prefix, e.g. 203.0.113.0/24>","agent":"<short browser and OS summary, at most 80 printable chars>","expiresAt":<unix ms>}`.
- Host to relay: `{"type":"approval-decision","id":"<same id>","approve":true|false}`. The relay accepts one decision per id, only from the host connection it sent the request to, and only before expiry.
- Relay to host: `{"type":"approval-cancel","id":"<same id>"}` when the request was decided elsewhere, expired, or the browser went away.
- The host shows one topmost dialog at a time, owned by the host process: "Allow a new browser to sign in?" with the client prefix, browser summary and remaining time, and the buttons **Allow** and **Deny**. It closes on cancel or expiry. Requests that arrive while a dialog is open are denied.
- Host approval proves presence at the PC, not identity. Anyone already viewing the desktop can see and click the dialog.

The fixture [`browser-approval-contract.json`](../../fixtures/browser-approval-contract.json) holds example messages; the Go relay, the Cloudflare relay and the host test against it.
