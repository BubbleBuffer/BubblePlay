# Follow active window

The browser's Monitors popover offers a default-off toggle when the host advertises `followWindow: 1`. The browser saves that choice and sends it on connection. Android currently has no toggle. Only the interactive user-level host advertises the capability; the older privileged-capture path does not.

## Host policy

The host samples the foreground window every 250 ms and chooses the monitor containing most of it. It ignores invisible, minimized, cloaked and shell windows. A changed window/monitor pair must appear in two consecutive samples; automatic switches happen at most once per second. A manual monitor selection therefore persists until a subsequent focus or placement change.

Ordinary following waits until the move/resize loop and left-button hold have ended. It uses the same switch path as a manual selection: `desktop-state` with `monitor-switch`, then `monitor-switched` and `monitor-list`. The first confirmed pair after enabling follow also counts.

The implementation also contains an edge-hold carry path: during a window move, holding the cursor within 2 px of an edge with an adjacent monitor for 400 ms switches the view and carries the cursor to the same relative point. It samples moves every 50 ms, preserves held input, excludes resizing and requires leaving the edge zone before rearming. This path has unit coverage; the retained development record does not establish live edge-hold acceptance.

## Wire contract

- `host-capabilities.followWindow` is an integer version, absent when unsupported.
- Client signaling sends `{"type":"media-control","action":"follow","follow":true,"protocol":4,"epoch":"…","reason":"user-follow"}`; `follow` must be boolean and the epoch current.
- `monitor-list.follow` reports host state when supported.
- Host state starts off for each signaling session; clients that never send follow controls retain ordinary monitor selection.

[`host/src/follow_window.rs`](../../host/src/follow_window.rs) contains the decision logic and Win32 sampler. It maps the chosen monitor by device name, falling back to monitor origin. The [shared fixture](../../fixtures/follow-window-contract.json) and browser/host tests cover the capability and messages. Focus-driven following was reported working on a two-monitor host; that observation is separate from edge-hold or an independent packaged installation.
