# Native automatic fallback — 2026-09-21

Phone report: desktop briefly appears, then waiting-for-stream and disappearance.
Installed host log around 21:44:26–21:44:29 UTC shows ICE connected and native
HEVC presentation, followed two seconds later by automatic WSS startup.

The automatic no-RTP timer checked only framesReceived, which the native video
callback did not update. The fix consults current-generation native presentation,
cancels the timer on an actual video frame, and updates native receive telemetry.
A silent connected peer still falls back; track announcement alone is insufficient.

Validation: 552 client tests passed (artifacts/native-auto-client-check.txt).
Local automatic-mode run 4b5e3ccf-1bb8-42ce-925d-32df2102abbb passed one full
monitor/transport cycle and a 30.33-second native soak, 35.84 presented fps,
maximum frame gap 67 ms. Its overall report remains unsupported because the
unelevated QoS query was unavailable. Owned processes exited, helper stopped,
and display cleanup succeeded. This is short regression evidence, not a new
full stability acceptance. The earlier ten-minute acceptance forced WebRTC
and did not exercise automatic fallback. Phone retest is still pending.

Worker deployment dry-run passed. No production deployment performed: production
still routes 100 percent to 46c1b9da-4d78-486f-84c0-27b65f537693. Publishing the
current branch would include earlier receiver/relay stabilization changes too.
Immediate workaround: https://playfull-cat.your-subdomain.workers.dev/?transport=webrtc

Wiki capture MCP unavailable; retained locally for later ingestion.
