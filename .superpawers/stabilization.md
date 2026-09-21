# BubblePlay stabilization

Approved approach: incremental cleanup before further optimization. Preserve wire
formats, CLI commands, service identities, authentication, queue bounds, event
ordering, retry timing, and streaming defaults. Production deployment is separate.

Baseline (2026-09-05, b1364fa): Node tests, configured JS syntax checks, and offline
Rust tests passed. npm's user-profile launcher is broken in the sandbox; direct
Node works. No live-stream baseline had been captured at planning time. The largest orchestration
boundaries are host run_serve, browser inline session state, and stream-lab-live.

Execution sequence:
1. Reproducible validation, setup/auth documentation, generated-file ignores.
2. Behavioral lifecycle tests and shared cross-language protocol fixtures.
3. Shared Rust library and focused host orchestration extraction.
4. Browser module boundaries and relay validation extraction.
5. Lab lifecycle/measurement separation and failure-safe cleanup.

After each extraction run affected tests; after integration run all deterministic
checks and build all Rust binaries. Keep behavioral fixes distinct from moves.
Do not claim optimization readiness until an isolated live candidate passes WSS,
WebRTC Gaming, H264 fallback, interruptions, child recovery, ten-minute soak,
cleanup/idle checks, and same-machine baseline comparison using existing gates.

## Implementation results (2026-09-05)

Implemented locally on `codex/stabilize-runtime`; no production deployment.

- Added a direct Node validation runner, setup/auth documentation, and generated-file ignores.
- Shared Rust modules through a library; extracted host capture, encoder, session policy,
  and benchmark helpers without changing the wire formats or streaming defaults.
- Moved the browser receiver into an external module, extracted the ACK timer with
  behavioral tests, and separated pure relay protocol validation.
- Added shared JSON protocol fixtures consumed by Rust and JavaScript tests.
- Separated lab lifecycle and measurement code. Fixed rejected-cleanup handling,
  receiver asset overrides after module extraction, and false-positive helper preflight.
- Corrected a gate test that assumed asynchronous notification arrival order.

### Evidence

Deterministic validation: `node tools/check.mjs full`; release build:
`cargo build --offline --locked --release --bins --manifest-path host/Cargo.toml`
with `NVIDIA_VIDEO_CODEC_SDK_PATH` pointing to `host/sdk-stub`.
Logs are retained locally in `artifacts/stabilization-check.txt` and
`artifacts/stabilization-build.txt`. Final full validation passed: 529 Node tests,
385 Rust tests, syntax checks for 65 scripts, all-target Rust checking, and Git
whitespace checks. Release builds passed with three existing library warnings.
Independent review found no remaining material
issues in the extracted boundaries after the asset/preflight fixes.

Live evidence: `artifacts/stabilization-live.json` and
`artifacts/stabilization-transport.json`. Baseline executable was built from
`b1364fa`; baseline and candidate used the same current lab/receiver and isolated
local relay. This is a host comparison, not a complete old/new client comparison.

| WSS measurement | Baseline | Candidate |
| --- | --- | --- |
| Presented fps | 30.07 | 30.05 |
| Latency p95 | 79 ms | 81 ms |
| Decoder resets | 0 | 0 |
| VMAF p5 | 80.054 | 80.028 |

These short samples show similar behavior; they do not establish statistical
equivalence. The optimizer's comparison reports no material quality improvement,
which is expected for a structural refactor and is not a stabilization acceptance
decision. An earlier run with a relay colocated in the synchronous measurement
process was invalid; it is retained as `stabilization-live-colocated-relay.json`
and excluded from the figures above.

The candidate completed a 600-second WSS soak at 29.99 fps, latency p95 78 ms,
zero decoder resets, and maximum frame gap 95 ms. Decoder recovery and one WSS
lane recovery completed. Fresh-session H.264 fallback completed on both builds.
Owned source/receiver/host processes exited gracefully and the lab helper stopped;
the Broker-owned persistent display remained present as intended.

### Remaining gates: optimization stays paused

The follow-up in [runtime-followup.md](runtime-followup.md) fixes the codec
contract and active RTP scheduling defects below and records the newer evidence.
The list here preserves the findings at the end of the initial structural pass.

1. WebRTC Gaming times out on both host builds with the receiver waiting for a
   startup keyframe. The lab forces `decoderPath=webcodecs` while requesting HEVC;
   `latency-worker.js` configures AVC and parses H.264 SPS/IDR. This capability
   mismatch is a likely cause, not yet a verified fix. Resolve the diagnostic
   contract and verify native HEVC WebRTC separately without weakening readiness.
2. H.264 following the failed WebRTC transition also times out, although fresh
   H.264 passes. Verify recovery from that failed transition after item 1.
3. Post-cleanup idle verification is unsupported because QoS evidence could not
   be queried. Process cleanup alone does not satisfy that gate.
4. The complete interruption/child-recovery acceptance matrix remains outstanding.

The host event loop and receiver session state still warrant incremental cleanup.
This pass establishes module boundaries and regression checks; it does not claim
that all internal coupling or runtime reliability work is finished.
