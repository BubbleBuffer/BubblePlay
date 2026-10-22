# BubblePlay relay (Rust)

The Rust relay is the native server for BubblePlay's host and client connections. This crate,
`relay-rs/server` (package `bp-relay-server`, binary `bp-relay`), is its shell: config loading,
the state files, the subcommands, the static asset handler and `/healthz`.

`serve` runs the relay state machine (`bp-relay-core`) and the site gate behind one axum router.
Every HTTP path and every WebSocket upgrade passes the gate first. The signalling, media and file
WebSocket routes (`/host-ws`, `/ws`, `/host-version-ws`, `/host-media-ws`, `/media-ws`,
`/host-file-ws`, `/file-ws`) go to the relay. `/healthz` and the `/__gate/manage/` admin API are
handled by the server itself. Static assets come from `assets_dir`.

## Configuration: relay.json

`serve` reads its config from `--config`, which defaults to `/data/relay.json`. Unknown fields
are an error. Every validation error names the field it is about.

| Field | Default | Notes |
| --- | --- | --- |
| `public_origin` | required | The public origin, such as `https://relay.example.com`. It is canonicalised, so `wss://` becomes `https://`. Plain `http` is allowed only for loopback hosts. |
| `listen` | `0.0.0.0:8080` | An IP address and port. Host names are not accepted. |
| `assets_dir` | `/assets` | The static files. |
| `state_dir` | `/data` | Where `pcs.json` and `gate.json` live. |
| `token` | required | The first-registration ticket. `BP_RELAY_TOKEN` overrides it. |
| `admin_token` | required | Bearer token for `/healthz` from outside loopback. `BP_RELAY_ADMIN_TOKEN` overrides it. |
| `max_connections` | `32` | At least 1. Enforced by the relay core, which is not wired in yet. |
| `approval_ttl_s` | `90` | Must be 30 to 120 seconds. |
| `trusted_proxy_cidrs` | `[]` | CIDR ranges of reverse proxies. `X-Forwarded-For` is read only when the direct peer is in one of them. |
| `turn` | none | Optional. `host`, `secret` (at least 32 bytes) and `ttl_s` (default `21600`, 300 to 86400). |
| `ntfy` | none | Optional. `url` (http or https), `topic` and `token`. Without it the gate has no approval path and a browser gets 503. Approval messages carry the approve and deny bearers in their action headers, never the poll token. Publishes use rustls with the bundled webpki roots and a 10 s timeout. |

Environment values win over the file. An empty value is an error. Secrets never appear in
logs or in `Debug` output.

## State files

Both files live in `state_dir`.

- `pcs.json` holds the registered host PCs: `{"version":1,"pcs":[{"id":"XXXX-XXXX-XXXX-XXXX","public":"<base64url 32 bytes>","registered_ms":0}]}`. Each ID must be the hash of its own public key.
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

Static assets follow the Go relay's rules. Only GET and HEAD are served. Paths are cleaned, so
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

## The `lab` feature

`lab` is a cargo feature. It is off by default, so a normal build has no lab code in it. The
feature compiles the loopback guard and turns on its check in `serve`.

    cargo build --release --features lab

With `lab` on, `serve` refuses to start unless every listen address is loopback: `127.0.0.0/8`
or `::1`. When it starts, it logs a loud warning. The shell has no lab routes yet. Never expose a
lab build.

## Migrating from the Go relay

1. Write a relay.json with `bp-relay init`, then set `public_origin`. Reuse the old token values if you want to keep them.
2. Run `bp-relay import-go-auth <auth.json> --config <relay.json>`. Only the `pcs` table is read. Passwords, TOTP secrets, sessions, rate counters and approvals are ignored on purpose. An entry is skipped and listed when its key is not the canonical device ID, its public key is malformed, its ID does not match its public key, or its `firstSeen` is not a positive time.
3. PCs registered on the Cloudflare Worker relay are not in the Go auth file, and they cannot be imported. Each of them re-registers once with `TOKEN` at cutover.

## M3 notes (Cloudflare build)

These three caveats came from the workers-rs spike.

- Use the stock release profile. A size-optimised profile (opt-level `s`, `lto`, `codegen-units = 1`, `strip`) breaks wasm-bindgen with "externref table required for catch wrappers".
- Override `websocket_close` and `websocket_error`. The workers-rs 0.8.7 defaults are `unimplemented!()` and panic.
- Repeat the p99 latency measurement on deployed Cloudflare. Local workerd showed a Rust p99 of about 1.2 s at 64 KiB, against 87 ms for JS.
