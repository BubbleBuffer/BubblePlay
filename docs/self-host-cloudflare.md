# Run your own Cloudflare relay

The existing relay serves the browser site, WebRTC signaling and WebSocket media/files/input. It needs a Cloudflare account and new-browser approval through ntfy notifications, a connected Windows PC, or both. [Go/Docker](self-host-docker.md) provides the alternative with authenticator codes; that path has separate runtime acceptance checks.

## Prepare the source

Install Node 22 or newer and use the repository's locked Worker dependencies:

```powershell
npm --prefix worker ci
cd worker
npx wrangler login
```

Edit `wrangler.toml`: choose your Worker name and `APPROVAL_METHODS`. Leaving it unset uses `ntfy`, preserving the existing default. Set `host` for approval on the PC, or `ntfy,host` to notify both. When ntfy is enabled, configure your own `NTFY_URL` and `NTFY_TOPIC`. Optional TURN configuration also belongs to your deployment; no TURN service is included.

Create three distinct random host/administration secrets with at least 32 characters, and a browser password with at least 12 characters. For example, generate each token locally:

```powershell
node -e "console.log(require('node:crypto').randomBytes(32).toString('base64url'))"
```

Store each credential through Wrangler's prompt. Do not paste real values into an issue, documentation or command arguments:

```powershell
npx wrangler secret put TOKEN
npx wrangler secret put HOST_GATE_TOKEN
npx wrangler secret put GATE_ADMIN_TOKEN
npx wrangler secret put APP_PASSWORD
npx wrangler secret put NTFY_TOKEN # required only when ntfy is enabled
npx wrangler deploy
```

`TOKEN` registers the Windows host. `HOST_GATE_TOKEN` protects native host endpoints. `GATE_ADMIN_TOKEN` protects browser-approval management. `APP_PASSWORD` authenticates the approved browser. `NTFY_TOKEN` authorizes notifications to your own ntfy service when that method is enabled. Missing or short required secrets cause a configuration error. Approval configuration also fails closed for an empty method list, an unknown method, or missing ntfy URL/topic/token when ntfy is enabled. Host-only requires no ntfy settings. There is no password-only mode.

Keep the returned HTTPS address. Install the Windows host with that address, the matching `TOKEN` and `HOST_GATE_TOKEN` using the [Windows quickstart](windows-quickstart.md).

Use a host version that announces `browserApproval: true` on `/host-ws` before
relying on host approval. Opening the site in a fresh browser starts a held
approval request. ntfy shows Approve/Deny actions; the PC shows an Allow/Deny
dialog. With both enabled, the first decision wins and the other is cancelled.
Approval releases the password form. Host approval proves presence at the PC;
someone viewing the desktop can approve. Existing timeout and session defaults
remain unchanged.

Host-only fails promptly when the PC is unreachable or lacks the capability.
Start or update the PC host, reconnect it, then reload the site. With both
methods enabled, ntfy remains available while the PC is offline. The login page
uses public `GET /__auth/methods` discovery to explain the request destination
and offers **Request approval instead** to a browser already approved.

## Check your deployment

Use a new browser profile to prove each configured approval method and password login, then verify the host registers and a desktop frame appears. For host-only, disconnect the PC and verify prompt failure before reconnecting it. For both methods, decide on one and verify the other request is cancelled. Test WebSocket fallback as well as WebRTC if that route is available. Copy text, use a native drop target and reconnect once. Check that a browser opened from another origin cannot reuse the authenticated WebSocket endpoints.

This guide describes the implemented Cloudflare path. A fresh independent Cloudflare-account deployment remains to be demonstrated before a public release.

## Operation

Use HTTPS/WSS for public connections. The normal host rejects a plain, non-loopback WebSocket relay URL. Browser sessions attach approval and login cookies; credentials do not belong in query strings. WebSocket media transport is encrypted to the relay, but the relay can inspect stream content: run it on infrastructure you trust. See [security and privacy](security-and-privacy.md).

Logout and browser revocation notify registered socket rooms immediately.
Active sockets recheck authentication with an approximately three-second
cache; idle sockets use an alarm scheduled every five seconds. Password rotation
and expiry are enforced on the next check. Durable Object scheduling is best
effort, so this interval is not a hard disconnect deadline.

Cloudflare cannot bound a slow peer's outgoing WebSocket queue because its API exposes neither `bufferedAmount` nor send completion; see the [workerd issue](https://github.com/cloudflare/workerd/issues/988).

Advanced setup, TURN and development-only stream-lab instructions remain in [the Worker README](../worker/README.md). Do not enable the stream-lab bypass on an internet-facing deployment.
