# BubblePlay signaling worker

One Cloudflare Worker: static hosting of the browser client + a WebSocket
relay that pairs exactly one host with one client.

## Deploy

```powershell
wrangler login
wrangler secret put TOKEN     # one shared secret; host and client must match
wrangler deploy
```

## Local dev

```powershell
Copy-Item .dev.vars.example .dev.vars   # set TOKEN
wrangler dev
```

Then open http://localhost:8787/?token=<TOKEN> in Chrome (Chromium required).
The host connects to the Worker's /ws endpoint:

```powershell
bubbleplay-host.exe --mode serve --worker ws://localhost:8787/ws --token <TOKEN>
```
