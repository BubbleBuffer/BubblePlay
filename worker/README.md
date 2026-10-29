# BubblePlay browser viewer

`worker/public/` is the browser receiver. The relay serves it from its static assets: the Rust relay in [`relay-rs/`](../relay-rs/README.md) (Docker and Cloudflare Workers). The JavaScript Worker relay that used to live here was deleted at CUT-1 (2026-10-25), and so was the stream lab that drove it (`worker/tools/`).

`worker/test/` holds the receiver tests. They run with `npm --prefix worker test`; the relay-side checks live in `relay-rs/`.

Relay setup: [relay-rs/README.md](../relay-rs/README.md).
