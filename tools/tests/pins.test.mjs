import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "../..");
const workflowDir = join(root, ".github/workflows");
const read = path => readFileSync(join(root, path), "utf8");
const workflows = readdirSync(workflowDir).filter(name => name.endsWith(".yml"))
  .map(name => ({ name, text: read(`.github/workflows/${name}`) }));

// Every distinct value found by `pattern` (first capture group) across all workflows.
function pinned(pattern) {
  const found = new Map();
  for (const { name, text } of workflows) {
    for (const match of text.matchAll(pattern)) found.set(match[1], [...(found.get(match[1]) ?? []), name]);
  }
  return found;
}
const onlyOne = (what, found) => assert.equal(found.size, 1, `${what} is pinned to different versions: ${JSON.stringify([...found])}`);

test("one Rust toolchain version across workflows", () => {
  const found = pinned(/(?:toolchain install|RUSTUP_TOOLCHAIN=|rustup default|rustc \+|--toolchain)\s*(\d+\.\d+\.\d+)/g);
  assert.ok(found.size > 0, "no Rust pin found");
  onlyOne("Rust toolchain", found);
});

test("one Flutter version across workflows", () => {
  const found = pinned(/FLUTTER_VERSION:\s*"?(\d+\.\d+\.\d+)"?/g);
  assert.ok(found.size > 0, "no Flutter pin found");
  onlyOne("Flutter", found);
});

test("wrangler is pinned to one version", () => {
  const found = pinned(/wrangler@(\d+\.\d+\.\d+)/g);
  assert.ok(found.size > 0, "no wrangler pin found");
  onlyOne("wrangler", found);
});

test("worker-build matches the worker crate", () => {
  const found = pinned(/WORKER_BUILD_VERSION=(\d+\.\d+\.\d+)/g);
  onlyOne("worker-build", found);
  const crate = /^worker\s*=\s*"=(\d+\.\d+\.\d+)"/m.exec(read("relay-rs/worker/Cargo.toml"))?.[1];
  assert.equal(crate, [...found.keys()][0], "worker-build must be the version of the worker crate");
});
