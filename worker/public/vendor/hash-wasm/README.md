# hash-wasm Argon2 build

- Version: **4.12.0** (MIT).
- Source package: https://registry.npmjs.org/hash-wasm/-/hash-wasm-4.12.0.tgz
- Source tarball: `hash-wasm-4.12.0.tgz` (`npm pack hash-wasm@4.12.0` on linux01).
- npm SHA-512 integrity: `sha512-+/2B2rYLb48I/evdOIhP+K/DD2ca2fgBjp6O+GBEnCDk2e4rpeXIK8GvIyRPjTezgmWn9gmKwkQjjx6BtqDHVQ==`

Before extraction, the tarball's computed SHA-512 was compared to both the
registry's `dist.integrity` and `npm pack --json`'s integrity; all matched.
`argon2.umd.min.js` is copied unchanged from `package/dist/argon2.umd.min.js`.
It contains the Argon2d/i/id implementation with embedded WASM, without the
package's unrelated hash builds. `LICENSE` is copied unchanged from
`package/LICENSE`. No npm install or build is needed to serve these assets.

The device-key module uses only Argon2id, in a dedicated module Web Worker,
with memorySize 65536 KiB, iterations 3, parallelism 1, hashLength 32 and binary
output. The upstream UMD build also works as a side-effect ES module import,
exposing `globalThis.hashwasm`. Each derivation disposes its Worker afterwards.
The project's main third-party notice entry is deferred to the main session.

The upstream build rejects empty passwords. The device-key API reports this
constraint as invalid input; omit the optional passphrase for a plain key file.
