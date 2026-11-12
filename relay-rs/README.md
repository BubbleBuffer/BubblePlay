# BubblePlay relay (Rust)

The Rust relay is the native server for BubblePlay's host and client connections. This crate,
`relay-rs/server` (package `bp-relay-server`, binary `bp-relay`), is its shell: config loading,
the state files, the subcommands, the static asset handler and `/healthz`.

`serve` runs the relay state machine (`bp-relay-core`) and the site gate behind one axum router.
Every HTTP path and every WebSocket upgrade passes the gate first. The signalling, media and file
WebSocket routes (`/host-ws`, `/ws`, `/host-version-ws`, `/host-media-ws`, `/media-ws`,
`/host-file-ws`, `/file-ws`) go to the relay. `/healthz`, `GET /presence` and the
`/__gate/manage/` admin API are handled by the server itself. Static assets come from `assets_dir`.

## Presence: GET /presence

`GET /presence?ids=A,B,C` tells a native client which PCs are online. It needs no cookie and no
bearer, and it has no side effects: it only reads whether a host socket for each PC is open (the
same state as `online` in the admin listing).

- Up to 20 device ids, comma-separated (`%2C` works too). More than 20, a missing or empty `ids`,
  or any malformed id is `400` with a JSON body such as `{"error":"too many ids"}`.
- `200 application/json`: one key per requested id in canonical form (`77VH-7M56-8GJK-Y12J`),
  `true` while that PC's host socket is open, else `false`. An unknown id is `false`.
- 60 requests per minute per client prefix (IPv4 /24, IPv6 /56, as the gate counts). Over that,
  `429` with `Retry-After` in seconds. The counter is in memory only.
- Every answer carries `Cache-Control: no-store`. Ids are not logged.
- The gate lets only a GET on exactly `/presence` through, before its navigation step, so a
  cookieless query never opens an approval, holds, publishes to ntfy or sets a cookie, whatever
  its `Sec-Fetch-Dest`. A request whose `Origin` is not `public_origin` is still refused (404), and
  there is no CORS: the route is for native clients. Other methods on the path get the gate's
  usual answers (cookieless POST 403, HEAD dropped).
- The Cloudflare build answers the route the same way from the Durable Object.

## Logging

Every refusal and close the relay core makes is logged as one JSON line (logging spec: `ts`, `level`,
`src` "relay", `device` null, `pc`, `session` null, `target` "relay", `msg`, `fields`). `fields` carries
`class` (a dotted reason class such as `pairing.wrong-code`, `viewer.not-admitted`, `timeout.hello`,
`ticket.invalid`, `packet.invalid-signal`, `restore.invalid`, `gate.refused`, `upgrade.bad-pc`), `status`
(the close code, or 404 for a refused upgrade), `reason` (the string the client got), `role` and, once known,
`peer` (the viewer's id). PC and viewer ids are cut to their first 8 characters. A 1008 close is `warn`,
any other close is `info`. Pairing codes, proofs, tickets, tokens and message contents are never passed to
the logger, and the logger redacts sensitive keys, `key=value` pairs, bearer values, 40+ character blobs and
the configured tokens as a net (`relay-rs/core/src/log.rs`).

The core has no console, so each adapter installs a sink once: the native server writes the line to stderr
(`server/src/log.rs`), the Worker to the console (`console.debug|log|warn|error` by level), where Workers
observability (`[observability] enabled = true`) collects it. Client-log WebSocket messages
(`type: "client-log"`) are still dropped, never forwarded; the relay logs a debug line `client-log
messages dropped` with a count only, at most once a minute. Clients send their logs to the PC over the
end-to-end session channel instead.

## Configuration: relay.json

`serve` reads its config from `--config`, which defaults to `/data/relay.json`. Unknown fields
are an error. Every validation error names the field it is about.

| Field | Default | Notes |
| --- | --- | --- |
| `public_origin` | required | The public origin, such as `https://relay.example.com`. It is canonicalised, so `wss://` becomes `https://`. Plain `http` is allowed only for loopback hosts. |
| `listen` | `0.0.0.0:8080` | An IP address and port. Host names are not accepted. |
| `assets_dir` | `/assets` | The static files. |
| `state_dir` | `/data` | Where `pcs.json` and `gate.json` live. |
| `token` | required | The first-registration ticket, at least 32 bytes (`MIN_SECRET_BYTES`). `BP_RELAY_TOKEN` overrides it. |
| `admin_token` | required | Bearer token for `/healthz` from outside loopback and the admin API, at least 32 bytes. `BP_RELAY_ADMIN_TOKEN` overrides it. |
| `max_connections` | `32` | At least 1. Enforced by the relay core. Pending sockets are capped separately (see below). |
| `approval_ttl_s` | `90` | Must be 30 to 120 seconds. |
| `trusted_proxy_cidrs` | `[]` | CIDR ranges of reverse proxies. `X-Forwarded-For` is read only when the direct peer is in one of them. |
| `turn` | none | Optional. `host`, `secret` (at least 32 bytes) and `ttl_s` (default `21600`, 300 to 86400). |
| `ntfy` | none | Optional. `url` (http or https), `topic` and `token`. Without it the gate has no approval path and an unapproved navigation is dropped. Approval messages carry the approve and deny bearers in their action headers. Publishes use rustls with the bundled webpki roots and a 10 s timeout. |

Environment values win over the file. An empty value is an error. Secrets never appear in
logs or in `Debug` output.

## State files

Both files live in `state_dir`.

- `pcs.json` holds the registered host PCs: `{"version":1,"pcs":[{"id":"XXXX-XXXX-XXXX-XXXX","public":"<base64url 32 bytes>","registered_ms":0,"admitted":["XXXX-XXXX-XXXX-XXXX"],"remotePairing":false}]}`. Each ID must be the hash of its own public key. `admitted` lists the viewer IDs the host admitted (at most 256, canonical and unique) and defaults to empty. `remotePairing` defaults to `false`.
- `gate.json` holds the gate cookies: `{"version":1,"cookies":[{"hash":"<base64url SHA-256 of the cookie value>","expires_ms":0}]}`. The cookie itself is never stored, only its hash. The gate's tick drops expired cookies and rewrites the file.

Writes are atomic. The new content goes to a temp file in the same directory, which is fsynced
and renamed over the old file. On unix the directory is fsynced too. Files are mode 0600 on unix.

A corrupt or inconsistent file stops the relay with an error. It is never reset to empty. A
missing file counts as empty.

## Subcommands

- `bp-relay serve [--config <path>]` runs the relay until SIGTERM or Ctrl-C. Log level comes from `RUST_LOG`.
- `bp-relay init <path>` writes an example relay.json with random `token` and `admin_token` values, mode 0600. It refuses to overwrite an existing file.
- `bp-relay healthcheck [--config <path>]` requests `/healthz` on the local listen address and exits 0 on `200`. The Docker HEALTHCHECK uses it, so the image needs no curl.
- `bp-relay import-go-auth <auth.json> [--config <path>]` copies the registered PCs from a Go relay auth file into `pcs.json`. See the migration section.

Static assets are served under these rules. Only GET and HEAD are served. Paths are cleaned, so
`..` cannot leave `assets_dir`. A path with a backslash is a 404. A dotfile, a directory or a
missing file is a 404. A request for `/index.html` redirects to `./`. Responses carry
`Cache-Control: no-store`, `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`.

## Docker

The build needs the repository root as its context, because it uses `crates/`, `relay-rs/` and
`worker/public/`. Run these commands from the repository root:

    docker build -f relay-rs/Dockerfile -t bp-relay .
    docker run --rm --entrypoint /bp-relay -v bp-relay-data:/data bp-relay init /data/relay.json
    docker run -d --name bp-relay -p 8080:8080 -v bp-relay-data:/data bp-relay

The image is a static musl binary on `scratch`. It runs as uid 10001 and keeps its state in
`/data`. Set `public_origin` in the generated relay.json to the public address before you start it.

The build context is the repository root. `relay-rs/Dockerfile.dockerignore` holds its ignore
rules: BuildKit reads a Dockerfile-specific ignore file that sits next to the Dockerfile, so it
applies to this build with the repo-root context. Keep it next to the Dockerfile.

## Viewer admission (M2)

A viewer's `/ws` hello is signed. The client sends `{"type":"hello","role":"viewer","id","public","turn","pair"?}`.
The relay answers `register-challenge` with a nonce. The viewer replies `register-proof` with a
signature over that nonce and the public origin, made with the viewer's device key.

The viewer role names are `viewer` on `/ws`, `media-viewer` on `/media-ws` and `file-viewer` on
`/file-ws`. The old names `client`, `media-client` and `file-client` are refused like any unknown role.

- An ID in the PC's `admitted` list is admitted. The relay sends `hello-ok` with a media ticket.
- Otherwise, while the host has an open pairing window, a viewer that sends the window's 32-byte `pair` value is admitted in pairing mode. Its `hello-ok` carries `pairing: true` and no media ticket. A pairing viewer may send only `auth-hello`, `auth-finish`, `auth-bind` and `pair-proof`, and the host may send it only `auth-host`, `auth-bind`, `pair-proof`, `pair-done` and `pair-denied`. Everything else is dropped in that direction.
- A pairing window opens with the host's `pair-open` and lasts 300 s. It closes on `pair-close`, when the host disconnects, or after 20 failed attempts against that PC. Five failures from one client prefix in 300 s refuse that prefix. The host gets `pair-closed` when a window closes on failures.
- Anything else is closed 1008 with a short plain reason a client can show: "invalid hello" (a malformed or unexpected hello or proof), "invalid proof" (the identity proof did not verify), "not paired" (not admitted and no pairing code), "pairing code not accepted" (wrong code, or no pairing window open), "rate limited" (too many failed pairing attempts from this network, or the prefix table is full), "timed out" (no hello or proof in time). No reason carries a secret.
- A browser viewer (one that sends an `Origin`) also needs a valid gate cookie. Without one it is closed 1008 "not approved".
- Origin-less sockets are native clients. They are upgraded and judged by the signed hello alone.

The host's `admit` message sets the admitted list. It has `viewers` (at most 256 canonical IDs, no
duplicates) and `remotePairing` (a boolean). The relay stores `remotePairing` with the PC and
reports it back, but admission does not read it. A viewer that drops off the admitted list is
closed 1008 "access removed". A pairing viewer is not affected by that check.

On `/ws` the relay forwards text only, and only these types:

- viewer to host: `auth-hello`, `auth-finish`, `auth-bind`, `pair-proof`, `ctl`;
- host to viewer: `auth-host`, `auth-bind`, `pair-proof`, `pair-done`, `pair-denied`, `ctl`.

In pairing mode only the `auth-*` and `pair-*` types above are forwarded, and `ctl` is not. The relay
answers `ping` with `pong` itself. The host's `admit`, `pair-open` and `pair-close` are consumed by
the relay and never forwarded. Any other type, including offer, ICE, capability and input messages,
is dropped and the socket stays open. Binary frames on `/ws` are dropped in both directions.
Media and file sockets are not affected by this list.

## Migrating from the Go relay

1. Write a relay.json with `bp-relay init`, then set `public_origin`. Reuse the old token values if you want to keep them.
2. Run `bp-relay import-go-auth <auth.json> --config <relay.json>`. Only the `pcs` table is read. Passwords, TOTP secrets, sessions, rate counters and approvals are ignored on purpose. An entry is skipped and listed when its key is not the canonical device ID, its public key is malformed, its ID does not match its public key, or its `firstSeen` is not a positive time.
3. PCs registered on the Cloudflare Worker relay are not in the Go auth file, and they cannot be imported. Each of them re-registers once with `TOKEN` at cutover.

## Known limits (M2)

- Navigations held for approval are capped: 4 per client prefix and 64 in total (`MAX_HELD_PER_PREFIX`, `MAX_HELD_TOTAL`). One over a limit is dropped at once, the same answer as an expired hold. The slot is returned when the hold ends or the request is cancelled.
- Pairing windows and pair-failure counters live in memory. A restart closes every window, and the host must send `pair-open` again.
- The admin listing at `/__gate/manage/pcs` reports the count of admitted viewers per PC (`admitted`), never the IDs.
- `last_seen` is derived, not stored: the PC table keeps `registered_ms` only, and on load `last_seen` is set equal to `registered_ms`. The admin listing shows a PC's last contact as its registration time until it connects again after a restart.
- Queued frames drain before a close: a socket's frames already in its queue are sent before its close frame, including the 1008 and 1001 closes. The drain shares `CLOSE_TIMEOUT` (2 s). A socket being removed or replaced can therefore receive frames queued just before the close.
- A failed-upgrade connection is reaped by `tick`: the core accepts an upgrade before the WebSocket handshake finishes. If the handshake fails, the connection stays until its hello deadline (5 s, `HELLO_TIMEOUT_MS`), and `tick` then closes it 1008 "timed out" and forgets it. A socket still in its hello or viewer proof at its deadline is closed the same way (the proof deadline is 10 s, `PROOF_TIMEOUT_MS`), and a host that misses its proof deadline is closed 1008 "invalid hello".
- Pending sockets are capped apart from the connection cap: 4 per client prefix and 8 per PC object (`PRE_AUTH_MAX_PER_PREFIX`, `PRE_AUTH_MAX_TOTAL`). A socket past either limit, or one that arrives when the object is at `max_connections`, drops the oldest pending socket (of the same prefix, or of the busiest prefix) with close code 1013 "too many pending connections" instead of being refused. Sockets that finished their handshake are never dropped for this, so unauthenticated clients cannot fill the cap. The connection cap refuses an upgrade only when every slot holds a socket that finished its hello.
- Admin DELETE of an unknown PC returns 404 (`NotFound`) and persists nothing. Deleting a registered PC persists first and returns 204.

## M3 notes (Cloudflare build)

These three caveats came from the workers-rs spike.

- Use the stock release profile. A size-optimised profile (opt-level `s`, `lto`, `codegen-units = 1`, `strip`) breaks wasm-bindgen with "externref table required for catch wrappers".
- Override `websocket_close` and `websocket_error`. The workers-rs 0.8.7 defaults are `unimplemented!()` and panic.
- Repeat the p99 latency measurement on deployed Cloudflare. Local workerd showed a Rust p99 of about 1.2 s at 64 KiB, against 87 ms for JS.

- Two Durable Object classes (2026-11-11). Every PC has its own session object (`PcRelay`, named `pc-<device id>`, binding `PC_RELAY`) that runs the core for that PC: its host and viewers' signalling, media, control and file sockets. Streams of different PCs never share an object, a connection cap or a CPU budget. The front object (`Relay`, `relay-v1`, binding `RELAY`) holds the site gate, the HTTP side (static assets, healthz, admin API, `GET /presence`) and a directory of registered PCs. The Worker sends a browser's WebSocket upgrade (it carries an `Origin`) to the front object for the gate check first, then to the PC's object; a native client has no `Origin` and goes straight to the PC's object. A PC's object reports its record, admitted count and the host's presence to the directory (`POST /__internal/dir/report`, reachable only from the objects), when it changes and every 4 minutes while the host is online; the directory treats a report older than 10 minutes as offline. The directory refuses a new PC at 4096. A PC's object that already stores its record writes locally first and reports to the directory afterwards, so a registered host registers even when the front object does not answer; a failed report is retried by the alarm with a doubling wait of 2 s up to 60 s (`REPORT_RETRY_BASE_MS`, `REPORT_RETRY_MAX_MS`). A PC the object has never stored is reported first, so the directory can refuse it at the cap. Admin delete asks the PC's object to drop the record, then removes the directory entry. A PC registered before the split keeps working without anyone re-registering: the front object still holds the old `pcs` table, and a PC's object that starts empty asks for that PC's record on its first upgrade (`POST /__internal/dir/claim`), saves it, and reports it, after which the front object deletes the old entry (and the `pcs` key when the table is empty).
- Hibernation: each socket's state is a `ConnSnapshot` in its attachment (2048 B limit). On wake the DO calls `Relay::rehydrate` and arms an alarm at `next_deadline()` instead of ticking. A wake loses the pending viewer buffer, per-prefix pairing failure counts, and the gate's pending approvals, cooldowns and decision counts. The gate fails closed. Sockets awaiting a hello or proof are closed 1008. The PC-wide pairing failure count survives. A socket the relay closes has its attachment cleared first: a closed socket stays in `getWebSockets()` until its peer answers the close, which a dead peer never does, and a wake must not restore it. When two snapshots claim the same host, viewer or data slot, rehydrate keeps the higher id, the socket that replaced the other (2026-10-30; before this, a wake kept the replaced socket and closed the live one 1008, which the viewer treats as final).

## Cloudflare build (M3)

`relay-rs/worker` (`bp-relay-worker`) is the Cloudflare Workers adapter. relay-rs excludes it from its workspace, and it has its own committed `Cargo.lock`. The `relay-rs` job in `.github/workflows/checks.yml` builds it with `worker-build --release`, checks it with clippy for wasm32 and `cargo fmt --check`, and runs `node --test tests/` against a local workerd started by `wrangler dev --local`. Nothing is deployed: the crate has no account, route or secret.

## Measuring p99 on deployed Cloudflare

Status: **not yet run.** Nothing below has been run against a deployed relay. The only measurements so far are the local-workerd numbers from the M0 spike.

The bench measures the ctl channel, viewer to host and back, with one message in flight at a time, at 1 KiB, 16 KiB and 64 KiB. The M0 spike measured one-way media frames under a 30 Mbit/s load. The two sets of numbers measure different things, so compare them with care.

M0 baseline, local workerd, 64 KiB: Rust p99 about 1175 ms, JS p99 87 ms (M0 spike verdict, 2026-10-22).

1. Copy the template and fill in the placeholders. Keep the copy out of git, because it holds your account's values:
   ```
   cd relay-rs/worker
   cp wrangler.deploy.toml.example wrangler.deploy.toml
   ```
2. Set the secrets. Each command prompts for its value. Generate fresh values for this run:
   ```
   npx wrangler@4.148.0 secret put RELAY_TOKEN -c wrangler.deploy.toml
   npx wrangler@4.148.0 secret put ADMIN_TOKEN -c wrangler.deploy.toml
   ```
   Add `TURN_SECRET` (32 bytes or more) and `NTFY_TOKEN` only if you set those vars.
3. Build and deploy:
   ```
   worker-build --release
   npx wrangler@4.148.0 deploy -c wrangler.deploy.toml
   ```
   Note the URL that deploy prints. The bench needs it.
4. Run the bench from a machine with Node 24. Pass the registration token in `BENCH_TOKEN`, not on the command line. In PowerShell, set it with `$env:BENCH_TOKEN = '...'`.
   ```
   BENCH_TOKEN=<RELAY_TOKEN value> node relay-rs/worker/scripts/bench-p99.mjs \
     --url https://<deployed URL> --origin https://relay.example.com --count 1000 > bench-deployed.json
   ```
   `--origin` must equal `PUBLIC_ORIGIN`, because the relay proof is bound to it. Without `--origin`, the origin of `--url` is used. Each run registers one throwaway PC, which stays in the relay's state until teardown. The output is JSON with p50, p95, p99 and max per size.
5. Compare with M0. Read the p50, p95 and p99 for each size from `bench-deployed.json` and set them beside the M0 numbers above. A large gap needs an explanation before anyone acts on it.
6. Tear down when you are done. Confirm the prompt:
   ```
   npx wrangler@4.148.0 delete -c wrangler.deploy.toml
   ```
