# Repository tools

Run these commands from the repository root.

| Tool | Purpose |
| --- | --- |
| `node tools/check.mjs client` | Check browser and Worker syntax, then run the client tests. |
| `node tools/lint/gates.mjs` | Lint gates, shrink-only: file length (500), suppression counts, ESLint. Baselines in `tools/lint/`; lower them with `--update` after fixing. Needs `npm --prefix worker ci` once. `clippy <project>`, `dart` and `detekt` gates run in CI jobs with those toolchains. |
| `node tools/check.mjs lint` | The same gates through the check runner (`full` includes them). |
| `node tools/check.mjs full` | Also run the host checks and inspect diff whitespace. |
| `tools/package-release.ps1` | Create a Windows tray ZIP from an explicit binary, script and document allowlist. See [release instructions](../docs/development.md). |
| `node tools/generate-notices.mjs` | Generate dependency notices using the overrides and license texts in `third-party/`. |
| `tests/` | Packaging and notice-generation regression checks. |
| `lab/verify-lab-qos.ps1` | Inspect Windows lab QoS cleanup and save a report under `artifacts/`; requires elevation. |

The supported Windows installer commands live in [host/scripts](../host/scripts/).
