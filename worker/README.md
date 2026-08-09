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

## Latency tests

The deterministic receiver tests have no third-party dependencies:

```powershell
npm test
```

The live runner occupies the room's single client slot, injects a 750 ms
main-thread stall, and writes a JSON report under `../artifacts/latency/`:

```powershell
$env:BUBBLEPLAY_TOKEN = "<TOKEN>"
npm run latency:live -- --url https://bubbleplay-signal.example.workers.dev/ --seconds 30
```

The software glass-to-glass mode briefly opens a full-screen timestamp barcode
on the captured primary display. It measures pattern draw -> DXGI -> NVENC ->
WebRTC -> WebCodecs -> receiver canvas; physical panel scanout is not included.

```powershell
npm run latency:bench -- --url https://bubbleplay-signal.example.workers.dev/ --seconds 30
```
