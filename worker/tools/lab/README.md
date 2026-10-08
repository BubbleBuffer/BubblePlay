# Stream lab

Development tools for measuring and diagnosing BubblePlay. These tools are not
part of the Windows release ZIP. Browser fixtures remain in `worker/public/`
because the receiver and local relay share those assets.

Install the Worker development dependencies with `npm --prefix worker ci`.
Run npm commands from `worker/`, or use `npm --prefix worker run <command>` from
the repository root. Direct invocations below start from the repository root.

Live commands require `--url https://your-relay.example.com/` or the `BUBBLEPLAY_URL` environment variable. Set this before running a stream or latency command; preflight, report and cleanup commands do not need a destination. No personal relay is selected by default.

Live commands require an explicit relay address through `--url <address>` or
`BUBBLEPLAY_URL`. For npm commands, pass flags after `--`, for example
`npm --prefix worker run lab:manual -- --url http://127.0.0.1:8787/`.
The tools do not select a deployed relay automatically.

| Entry point | Purpose |
| --- | --- |
| `npm --prefix worker run lab:preflight` | Report stream campaign prerequisites. |
| `npm --prefix worker run lab:manual` | Run an interactive candidate stream session. |
| `npm --prefix worker run lab:baseline`, `lab:smoke`, `lab:sweep`, `lab:campaign`, `lab:optimize` | Run measurement and encoder experiments. |
| `npm --prefix worker run lab:compare`, `lab:cleanup` | Compare saved runs or end a lab session. |
| `npm --prefix worker run lab:direct-relay` | Start the local loopback relay used by lab clients. |
| `npm --prefix worker run latency:live`, `latency:bench` | Collect live and glass-to-glass latency evidence. |
| `npm --prefix worker run recovery:inventory` | Save a workspace and runtime checkpoint under `artifacts/recovery-inventory/`. |
| `node worker/tools/lab/native-hevc-acceptance.mjs` | Exercise native HEVC decode and idle behavior. |
| `node worker/tools/lab/stream-lab-v4-check.mjs`, `stream-lab-v4-desktop.mjs` | Inspect V4 media and desktop rendering. |
| `node worker/tools/lab/stream-lab-file-transfer.mjs`, `stream-lab-native-file-drop.mjs` | Exercise transfer throughput and native desktop drops. |
| `node worker/tools/lab/file-drop-preview-lab.mjs` | Capture browser file-drop preview states using a local fixture. |
| `node worker/tools/lab/stream-lab-chart.mjs`, `stream-lab-history-chart.mjs` | Build reports from saved artifacts. |

The remaining modules implement campaign state, measurement, lifecycle,
physical-activity guards and helper-service communication. Some live commands
start local hosts or browsers and require Windows helper services, media tools
or capture hardware; use [the Worker guide](../../README.md) for prerequisites
and acceptance limits. Host-side installation and maintenance scripts remain in
`host/scripts/`. The elevated, read-only QoS verifier is
`tools/lab/verify-lab-qos.ps1` at the repository root.
