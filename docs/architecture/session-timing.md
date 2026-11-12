# Session timing

[`fixtures/session-timing-contract.json`](../../fixtures/session-timing-contract.json) records every timer and retry schedule of the signaling and media links, per implementation: browser, Kotlin (Android), client-win (also Flutter Windows), host and relay. The values are the ones the code has today. They are not decided values: where implementations differ, each keeps its own number under its own key, and the file's `disagreements` list names the keys that are open for the owner to decide.

Each implementation has one test that reads its section and fails when the code differs:

| Implementation | Test |
| --- | --- |
| Browser | `worker/test/timing-contract.test.mjs` |
| Kotlin | `android/app/src/test/java/de/bubblebuffer/bubbleplay/net/TimingContractTest.kt` |
| Relay | `relay-rs/core/tests/contract_timing.rs` |

A key in a section's `unpinned` object has no reader yet; its value names what blocks it (a literal in a source file another change is editing, or a private constant). The delay schedules in `schedules` are the table-driven check for backoff, recycled-session auth waits and the reconnect socket-open timeout: attempt n gives one delay in every implementation that uses the schedule.

Changing a timer means changing the fixture and the code together. A disagreement is resolved by giving the implementations the same value in the fixture, not by adding a second copy.
