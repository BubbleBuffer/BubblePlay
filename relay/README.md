# BubblePlay Go relay

One Go service serves the existing public browser assets and pairs one Windows host with one viewer per PC room. It supports WebSocket video/input/files and forwards WebRTC SDP/ICE. It requires no Cloudflare, Node runtime or database. The public product does not include the separate private execution companion.

For the operator walkthrough, use [Go/Docker self-hosting](../docs/self-host-docker.md) and the [Windows quickstart](../docs/windows-quickstart.md). This README keeps configuration, administration and development details. The browser source stays in `worker/public/` and is shared by both relay implementations; see [architecture](../docs/architecture/README.md) and [release status](../docs/release-status.md).

Browser login is **application password + authenticator code**, or **application password after approval on the PC or by ntfy notification**. The operator chooses which methods to offer; there is no password-only mode. A previously approved remembered browser can use its approval cookie with the password. TOTP follows RFC 6238: six SHA-1 digits, 30-second steps, a ±1-step skew allowance, and an atomically persisted highest consumed step. A used or older code cannot log in again. Keep the server clock synchronized.

## First setup

Use a public HTTPS origin on your own domain. Run exactly one relay replica and preserve its private state volume. In this example the reverse proxy runs on the Docker host and targets `http://127.0.0.1:8787`. It must preserve Host, set `X-Forwarded-For` from the client connection, support WebSocket upgrades and allow the 90-second approval navigation. It terminates TLS; the container's port is published only on the Docker host's loopback interface.

From the repository root, on a machine with Docker already running:

```sh
docker build -f relay/Dockerfile -t bubbleplay-relay:local .
docker compose -f relay/compose.example.yaml run --rm relay init --state-dir /data --public-url https://play.example.com --assets-dir /assets --methods totp --trusted-proxy 172.30.0.0/24
docker compose -f relay/compose.example.yaml create relay
docker compose -f relay/compose.example.yaml cp relay:/data/provision.txt ./relay/provision.txt
docker compose -f relay/compose.example.yaml up -d
```

The setup command asks for a 12–256-character password without echo. It generates independent 32-byte registration/host/admin credentials and, only when `totp` is enabled, an authenticator seed and enrollment details. It writes `/data/config.json` and `/data/provision.txt` privately and prints only their paths. The `create` and `cp` steps export the provisioning file from the named volume without requiring a shell in the image. Keep the exported file private (mode 600 on Linux, an account-restricted folder on Windows). With TOTP enabled, open it locally to enroll its `otpauth://` URI or Base32 secret in an authenticator. Importing an enrollment URI must happen in your authenticator, not by visiting it on an unrelated website. Do not commit, publish or paste either private file into logs.

The named volume belongs to container UID 10001. To read/edit its private files, use a separate operator container with the same volume; the runtime image deliberately contains no shell. For example, an operator can use `docker compose ... run --rm --entrypoint /bubbleplay-relay relay rotate ...` for credential rotation, while configuration edits can use a trusted local utility container. A bind-mounted directory instead needs ownership by UID/GID 10001 and directory/file permissions 700/600 on Linux. Windows native CLI users should place state in a folder restricted to their account; Unix mode bits do not implement Windows ACLs.

Configure the existing Windows tray with the provisioned `wss://play.example.com/host-ws`, `TOKEN`, and `HOST_GATE_TOKEN`. Tokens are not URL query parameters. The tray's existing setup script stores them using CurrentUser DPAPI. Open `https://play.example.com/` in Chromium. With TOTP enabled, enter the password and current authenticator code. If a notifier is enabled, the page also offers approval on the PC, by notification, or both according to the configuration. Approve that request, then enter the password. Without TOTP, opening the site starts approval directly. Each decision approves only that pending browser navigation.

## Configuration and ntfy

`sourceURL` defaults to `https://github.com/BubbleBuffer/BubblePlay` and is returned by `/__auth/methods` for the browser and Android About links. Operators of a modified relay must point it at their modified Corresponding Source, as required by AGPL-3.0-or-later. Use an HTTP(S) source URL without embedded credentials.

`config.json` fields are lowerCamelCase. `publicURL`, `assetsDir`, `stateDir`, `token`, `hostGateToken`, `adminToken`, `passwordHash` and at least one approval method are required. Setup fills these. `approvalMethods` is a non-empty subset of `totp`, `ntfy`, `host`; `init --methods totp,host` selects the methods and the default is `totp`. Unknown or empty method lists are rejected. Older configs without this field derive their methods from their existing TOTP secret and ntfy configuration. Default internal `listen` is `:8787`; `maxConnections` is 32 and the maximum is 128. `approvalTTLSeconds` is 30–120, default 90.

To enable ntfy alongside TOTP, use `--methods totp,ntfy --ntfy-url https://ntfy.example.com --ntfy-topic bubbleplay-approvals` in the first `init` command. It asks for the ntfy publishing token and application password without echo; never put the token in a command argument. `--ntfy-token-file` accepts a private file for noninteractive native setup. The equivalent config fields are `ntfyURL` (HTTPS JSON publishing endpoint, usually the server root), `ntfyTopic`, and `ntfyToken`; all three are required when `ntfy` is enabled. Keep TOTP enabled if you need login without a connected PC or notification service. ntfy Approve/Deny actions POST independent, single-use bearer capabilities to `/__gate/decision`; neither the application password nor host credentials are sent to ntfy. Missing/failed notifications never grant approval. There are at most ten pending approvals and one per client prefix; requests expire or cancel on disconnect.

With `host` enabled, the Windows host must announce `"browserApproval": true` in its `host-capabilities` message on `/host-ws`. The relay sends `approval-request` messages only to that capable connection and accepts its `approval-decision` only for the same request before expiry. When both notifiers are enabled, they receive the request together: the first Allow or Deny wins. A decision elsewhere closes the PC prompt and invalidates the ntfy decision links; the notification itself may remain visible. `approval-cancel` also closes a host prompt on expiry or browser disconnect. See [the browser approval protocol](../docs/architecture/browser-approval.md) for message fields and dialog behavior. Host approval requires someone at the PC; it does not prove that person's identity.

Unauthenticated `GET /__auth/methods` returns `{"totp":true,"approval":["ntfy","host"],"hostOnline":false,"sourceURL":"https://github.com/BubbleBuffer/BubblePlay"}` with `Cache-Control: no-store`; values reflect the enabled methods and whether a capable host is connected. The approval navigation uses `approval=request`; `approval=ntfy` remains a compatibility alias for the same configured notifier flow. The page adapts its approval link to the available methods. A host-only request with no capable host online returns HTTP 503; it cannot silently approve or fall back to password-only login.

`trustedProxyCIDRs` defaults to empty: `X-Forwarded-For` is ignored. Repeat `init --trusted-proxy CIDR` for each trusted proxy address or network. With no trusted proxy configured, the relay warns once on the first request carrying `X-Forwarded-For` or `Forwarded`; configure the actual path so client-prefix rate limits and approval prompts identify the browser rather than the proxy.

The Compose example fixes its Docker bridge at `172.30.0.0/24`, gateway `172.30.0.1`. A host reverse proxy reaching the loopback published port normally appears inside the relay container as that bridge gateway, so the walkthrough trusts the bridge subnet, not `127.0.0.1/32`. Check for conflicts with your LAN, VPN and other Docker networks before creating it; if you change the subnet, change `--trusted-proxy` too. Verify the network gateway and the relay's observed peer on your Docker platform; trusting the actual gateway as `/32` is narrower when that address is stable. Only attach trusted services to this bridge.

For a proxy container reaching the relay directly, trust that proxy's container address or its dedicated proxy network instead. For a native relay reached directly by a host loopback proxy, trust `127.0.0.1/32` (and `::1/128` if used). For a separate proxy machine, trust its actual source address or narrowly scoped network as seen by the relay. Do not trust all client networks. Client identity is selected right-to-left through trusted proxy hops; forwarding headers are honored only from a trusted directly connected peer. Configure each proxy to set or append the client address and accept any incoming forwarding chain only from its own trusted upstream proxies. The configured HTTPS origin, rather than forwarded Host/proto, decides Origin policy and callback URLs. Host mismatches and foreign browser Origins are refused. Native host/Android sockets may omit Origin.

Optional `turnHost` (`hostname` or `hostname:port`), `turnSecret` and `turnTTLSeconds` enable your coturn REST credentials only when the client opts in. Default WebRTC uses Google STUN and otherwise falls back to WSS. The Go relay does not implement TURN or encode WebRTC video. WebRTC media flows directly/through configured TURN; WSS media/input/clipboard/files are visible to your relay above TLS.

## Rotation, revocation and restart

```sh
docker compose -f relay/compose.example.yaml stop relay
docker compose -f relay/compose.example.yaml run --rm relay rotate --state-dir /data --password --totp
docker compose -f relay/compose.example.yaml up -d
```

Use either flag to rotate only that factor. The updated provisioning file is replaced atomically with private permissions. Restart loads the new config; factor changes invalidate browser sessions and prevent old OTP replay state from affecting a new seed. Password-only rotation preserves authenticator replay protection. Existing ntfy-approved cookies are managed separately from password sessions; a configured factor change invalidates both during restart.

To change offered methods, stop the relay and run `rotate --state-dir /data --methods totp,host`, then restart. When adding ntfy, supply `--methods totp,ntfy,host --ntfy-url https://ntfy.example.com --ntfy-topic bubbleplay-approvals` and enter its publishing token at the prompt, or use `--ntfy-token-file` with a private file. Enrollment details are emitted only while TOTP is enabled; enroll any new seed from the replaced provisioning file. Export that file again after rotation using the first-setup `cp` command.

The admin bearer authenticates existing `/__gate/manage/sessions` and `/__gate/manage/auth-sessions` list/delete routes, individual gate-session revocation, and `/__gate/manage/password` for runtime password rotation. Never put it in a URL. Browser logout invalidates its application session; **Forget browser** invalidates approval too. Active browser sockets recheck authorization on every incoming packet and at least every five seconds while idle, so revocation closes the viewer and invalidates related epoch sockets. Runtime password rotation is persisted across restart until the provisioned config hash changes explicitly.

`auth.json` contains hashes, bounded session/rate/replay state and host PC registrations. It uses synchronized atomic replacement; no external database is needed. Protect and back up config and state together. Do not run multiple replicas against one directory. Active sockets/media/file operations are volatile and end on relay restart. Media ACKs, files, input and old-epoch control are never replayed. Only the public protocol's bounded pre-host SDP/ICE and idempotent start/monitor/follow requests may wait for the currently connected viewer's host.

## Host PC registrations

ID hosts connect to `/host-ws?pc=<id>` and send `id` and `public` (unpadded base64url, 32 bytes) in their hello. The URL and hello IDs must agree after canonicalization, and match the key's derived ID. The relay sends a fresh `register-challenge` nonce; the host signs the relay-proof encoding and returns `register-proof`. First registration requires `TOKEN`; remembered PCs can reconnect with their signature alone. A supplied wrong token is always refused. Signaling upgrades still require the `HOST_GATE_TOKEN` bearer. ID-less hosts keep using `/host-ws` and the legacy room.

Each PC has its own viewer/epoch. Viewers target `/ws?pc=<id>` or use the relay's default: newest registered online PC, then an online legacy host, then the newest listed PC (including a sole offline PC), then the legacy room. One shared resolver applies this rule to pc-less viewer `/ws`, `/media-ws` and `/file-ws`. Old Android viewers send only the signaling epoch on media/file URLs, without `pc` or tickets; the resolved room must still own that active viewer epoch, or the socket is refused. A changed default cannot cross-connect it to another PC. Signaling `hello-ok` includes a ticket for media/file hellos; current data URLs carry `pc` and `epoch`. Explicit `pc` and ticket behavior is unchanged. Host data sockets accept a bearer or host ticket. Viewer cookies remain required. Tickets expire with their signaling socket, at most 12 hours.

Use the `ADMIN_TOKEN` bearer for:

- `GET /__gate/manage/pcs`: lists `id`, `public`, `firstSeen`, `lastSeen` (Unix milliseconds) and `online`.
- `DELETE /__gate/manage/pcs/<id>`: removes the PC and closes its host sockets (204); registering again requires `TOKEN`.

The optional `pcs` map in version-1 `auth.json` holds at most 4096 PCs and survives password/factor changes. Keep tickets and tokens out of URLs.

## Native development and validation

Go module minimum is 1.26; shipping/CI toolchain is patched Go 1.27.1. Gorilla WebSocket is pinned to 1.5.3. Argon2id password hashing uses pinned `golang.org/x/crypto`; third-party notices accompany the binary/container. The Dockerfile-specific build-context allowlist includes only public sources, browser assets and licenses, and excludes local artifacts and operator state.

```sh
cd relay
go test -race ./...
go vet ./...
node tools/generate-contract-fixtures.mjs  # development only; regenerate browser packet fixtures
CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -o bubbleplay-relay .
```

Local HTTP is development-only: `init --state-dir <private-dir> --public-url http://127.0.0.1:8788 --listen 127.0.0.1:8788 --insecure-loopback --assets-dir ../worker/public`. Login cookies remain Secure, so browser behavior on loopback must be verified; use an HTTPS proxy for deployed use. `--password-file` supports noninteractive setup with a private local file. There are no development auth bypasses, private Agent routes, or Cloudflare lab/version APIs. `/healthz` provides a nonsecret liveness response. `bubbleplay-relay healthcheck --config PATH` probes it on the configured listen address; the Compose example uses that command as its container healthcheck. `--config PATH` selects the saved config.

Automated tests exercise synthetic HTTPS/auth and real local WebSocket pairs, including browser-generated packet fixtures, epochs, file bytes/disconnects, active revocation, TOTP replay/skew/concurrency/persistence, and fixture-driven host/ntfy approval. Approval tests cover capability gating, connection-bound decisions, first-decision wins, expiry/disconnect cancellation, method discovery, and configuration/provisioning changes. Proxy tests cover separate client-prefix lockouts and forwarding-header warnings. These do not establish a real Windows host/Chromium streaming demonstration, restrictive-network WebRTC/TURN acceptance, or an independent operator's Docker deployment. The image was built and run on a Linux Docker host on 2026-10-12 (see [Go/Docker self-hosting](../docs/self-host-docker.md)). Those remain explicit release acceptance checks.
