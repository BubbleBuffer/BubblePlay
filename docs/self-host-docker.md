# Run your own Go/Docker relay

The standalone Go service contains the browser site, authentication, WebRTC signaling and WebSocket video/input/file relay. It needs no Cloudflare account, Node runtime or database. Windows capture and encoding run on the native host.

The implementation has automated authentication and protocol tests, plus a desktop-browser login smoke test. The development machine's Docker engine is offline: an actual container run and native desktop stream against this relay remain to verify before public release.

## Prepare hosting

Use a machine with Docker already running and an HTTPS reverse proxy on your own domain. This walkthrough runs the proxy on the Docker host. Point it at `http://127.0.0.1:8787`, preserve the incoming Host, set `X-Forwarded-For` from the client connection, support WebSocket upgrades, and allow a browser-approval request to remain open for 90 seconds. The public URL you configure must match the browser's HTTPS origin. The Compose example publishes only the relay's loopback port.

The example fixes its Docker bridge at `172.30.0.0/24` with gateway `172.30.0.1`. Check that this subnet does not overlap your LAN, VPN or another Docker network. The container normally sees connections through the host's published loopback port as coming from the bridge gateway, so the setup below trusts that bridge subnet. If you change the subnet, change the trusted-proxy option too. Verify the gateway and observed peer address on your Docker platform; you can narrow the trust to the gateway's `/32` once confirmed. Only attach trusted services to this network.

Run one relay replica and keep its private state volume. Synchronize the server clock for authenticator codes. Choose any non-empty combination of `totp`, `ntfy` and `host`; the default is `totp`. An ntfy service is optional. Host approval needs a connected Windows host that supports browser approval. Keep TOTP enabled if you need a login path without either notifier. TURN is also optional and supplied separately.

## Create credentials and choose approval methods

From the repository root:

```sh
docker build -f relay/Dockerfile -t bubbleplay-relay:local .
docker compose -f relay/compose.example.yaml run --rm relay init --state-dir /data --public-url https://play.example.com --assets-dir /assets --methods totp --trusted-proxy 172.30.0.0/24
docker compose -f relay/compose.example.yaml create relay
docker compose -f relay/compose.example.yaml cp relay:/data/provision.txt ./relay/provision.txt
```

Replace the example origin with your own. Setup asks for a password without echo and generates separate host registration, endpoint and administration credentials. It stores a password hash, not the password. Only when `totp` is enabled does it generate an authenticator seed and include its enrollment URI and Base32 secret in the private provisioning file. Add that secret to an authenticator using six-digit codes, SHA-1 and a 30-second period. Open this file locally. Keep it private and out of backups intended for publication; its default repository path is ignored by Git and the Docker build context.

Use `--methods totp,host` to also offer approval on the PC, or `--methods host` to require PC approval for new browsers. `approvalMethods` in `/data/config.json` stores this selection. Older configs without this field retain their methods derived from their existing authenticator secret and ntfy configuration.

To enable ntfy alongside the authenticator **during initial setup**, add these options to the `init` command:

```sh
--methods totp,ntfy --ntfy-url https://ntfy.example.com --ntfy-topic bubbleplay-approvals
```

Replace the existing `--methods totp` option with this list; use `totp,ntfy,host` to offer all three. Setup also prompts for the publishing token without echo; `--ntfy-token-file` accepts a private file for noninteractive setup. Enabling ntfy requires its URL, topic and token. The ntfy endpoint receives expiring Approve/Deny capabilities. Host credentials and the application password are not sent to ntfy.

Repeat `--trusted-proxy CIDR` if your deployment has multiple trusted proxy hops. Forwarding headers are ignored from untrusted peers. With no trusted proxy configured, the relay warns once on the first request carrying `X-Forwarded-For` or `Forwarded`. For a proxy container reaching the relay directly, trust its actual container address or dedicated proxy network. For a native relay with a host loopback proxy, trust `127.0.0.1/32` and, if used, `::1/128`. For a separate proxy machine, trust its source address as observed by the relay. Each proxy must set or append the client address and accept incoming forwarding headers only from its trusted upstreams; do not trust all client networks.

Start the relay:

```sh
docker compose -f relay/compose.example.yaml up -d
```

## Connect the Windows host and browser

Use the provisioning file's `TOKEN` and `HOST_GATE_TOKEN` with the [Windows quickstart](windows-quickstart.md). The host endpoint is `wss://play.example.com/host-ws`; the installer also accepts the base HTTPS origin. Host credentials do not belong in query parameters.

Open your HTTPS origin in Chromium. With TOTP enabled, enter the application password and current authenticator code. If a notifier is enabled, the page also offers approval on the PC, by notification, or both. Without TOTP, opening the site starts approval directly. Approve the PC dialog or notification and then enter the password. When both notifiers are enabled, both receive the request; the first Allow or Deny wins. A decision elsewhere closes the PC prompt and invalidates the ntfy links, though the notification may remain visible. Expiry or browser disconnect also cancels the PC prompt. A remembered browser keeps its approval for up to 30 days; signing out retains approval, while forgetting the browser requires a fresh code or approval.

PC approval uses the host's `browserApproval` capability on its existing `/host-ws` connection. A host-only request returns HTTP 503 when no capable host is online. Unauthenticated `GET /__auth/methods` reports the available methods and whether a capable host is online, with `Cache-Control: no-store`. Approval links use `approval=request`; the older `approval=ntfy` URL remains an alias for the same enabled notifier flow. See [the browser approval protocol](architecture/browser-approval.md) for message details. Someone with access to the PC can accept its prompt; host approval establishes presence there, not identity.

Each accepted time step can be used once, with one step of clock skew allowed. If a code was already consumed, wait for the next code. The relay retains replay and revocation state across restart. Active sockets check revocation before incoming packets and on a five-second idle check.

## Operation and recovery

Keep `/data` across updates; it holds private configuration and authentication state. A restart ends active streams and file transfers. Do not share a state directory between replicas. The runtime image runs as an unprivileged user and contains no shell.

To rotate the password and authenticator seed:

```sh
docker compose -f relay/compose.example.yaml stop relay
docker compose -f relay/compose.example.yaml run --rm relay rotate --state-dir /data --password --totp
docker compose -f relay/compose.example.yaml cp relay:/data/provision.txt ./relay/provision.txt
docker compose -f relay/compose.example.yaml up -d
```

Use either rotation flag on its own when appropriate. Enroll the replacement seed before expecting its codes to work. Back up private configuration and state together. Windows native setup needs an account-restricted state folder; Unix file permissions do not substitute for Windows ACLs.

To change the offered methods, stop the relay and use `rotate --state-dir /data --methods totp,host` in the same `docker compose ... run --rm relay` command, then export the updated provisioning file and restart. When adding ntfy, use `--methods totp,ntfy,host --ntfy-url https://ntfy.example.com --ntfy-topic bubbleplay-approvals` and supply its token at the prompt or through `--ntfy-token-file`. TOTP enrollment details are included only when that method is enabled. Method changes invalidate existing browser sessions and approvals when the relay restarts.

Advanced proxy, ntfy, TURN, session-administration and development options are documented in [the relay README](../relay/README.md). See [security and privacy](security-and-privacy.md) for relay visibility: TLS protects each WSS connection, while the relay can inspect its payloads.
