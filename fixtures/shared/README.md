# Shared device-key fixture (vendored)

`device-keys-contract.json` is a byte-for-byte copy of `fixtures/device-keys-contract.json` from
https://github.com/BubbleBuffer/bubblebuffer-identity at tag `v0.1.0` (commit
`a539c7e8ba505734e9b0bae1a23a4eb5c3ef7f6e`), the version of the `bubblebuffer-device-key` crate
that `crates/bp-device-key` depends on. SHA-256 with LF line endings:
`9d19065ff1a4ef5135e6a9974cf94d455f9574fe01eae5254e3bc6b02f7ca301`.

Do not edit or regenerate it here. Update it only together with the crate tag, by copying the
file from the new tag. `node tools/device-keys/gen-contract.mjs --check` checks it against the
JavaScript implementation without rewriting it.

It has three sections, all in the shared `bubblebuffer-device-key` format and with neutral relay
labels: `ids`, `keyFiles` and `relayProof`. BubblePlay's own vectors (relay proofs with the label
`bubbleplay-relay-v1`, the handshake and phase 2) stay in `fixtures/device-keys-contract.json`.
