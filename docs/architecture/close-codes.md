# Close codes

[`fixtures/close-codes-contract.json`](../../fixtures/close-codes-contract.json) lists every WebSocket close the relay and the viewers send (code, exact reason string, sockets, cause), what each implementation does on receiving a code, and what the viewer shows. It records today's behaviour; nothing in it is decided.

Today 1008 (policy) carries every refusal. All viewers end the signaling session on it, whatever the reason, and describe it as removed or not admitted. That includes `peer replaced`, which is benign: a newer viewer took the PC's session. Every other code (1000, 1001, 1005, 1006, 1011, 1013) is retried with the implementation's own backoff, including 1013 `too many pending connections`, sent to a socket that had not finished its hello when the relay needed room.

On the media socket the implementations differ: the browser and Kotlin reconnect on 1008, client-win restarts the whole attempt, and the host retries until its refusal limit. The host also ends a media link for good when the relay's reason text contains `stale` or `retired`.

Tests that read the fixture:

| Implementation | Test |
| --- | --- |
| Browser | `worker/test/close-codes-contract.test.mjs` |
| Kotlin | `android/app/src/test/java/de/bubblebuffer/bubbleplay/net/CloseCodesContractTest.kt` |
| Relay | `relay-rs/core/tests/contract_close_codes.rs` |

client-win and the host have no reader yet (see `unpinned` in the fixture).
