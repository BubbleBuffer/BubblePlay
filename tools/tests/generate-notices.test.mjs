import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import crypto from "node:crypto";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const generator = fileURLToPath(new URL("../generate-notices.mjs", import.meta.url));
const hash = bytes => crypto.createHash("sha256").update(bytes).digest("hex");
function removeFixture(dir) {
  const absolute = path.resolve(dir), temporaryRoot = path.resolve(os.tmpdir()) + path.sep;
  assert.equal(absolute.startsWith(temporaryRoot) && path.basename(absolute).startsWith("bubbleplay-notice-test-") && !fs.lstatSync(absolute).isSymbolicLink(), true, "cleanup stays inside its generated temp fixture");
  fs.rmSync(absolute, { recursive: true, force: true });
}
function fixture() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bubbleplay-notice-test-"));
  const write = (name, bytes) => { const full = path.join(dir, name); fs.mkdirSync(path.dirname(full), { recursive: true }); fs.writeFileSync(full, bytes); return full; };
  const pkg = (name, vcsPath) => ({ id: name, name, version: "1.0.0", license: "MIT OR Apache-2.0", repository: "https://example.org/public-library", manifest_path: write(`${name}/Cargo.toml`, "# fixture") });
  const parent = pkg("upstream", ""), child = pkg("child", "child");
  for (const [name, vcsPath] of [["upstream", ""], ["child", "child"]]) write(`${name}/.cargo_vcs_info.json`, JSON.stringify({ git: { sha1: "a".repeat(40) }, path_in_vcs: vcsPath }));
  write("upstream/LICENSES/MIT.txt", "Synthetic license fixture\nCopyright 2000 Original Fixture Author\n");
  write("upstream/NOTICE", "Synthetic retained attribution notice.\n");
  const metadata = write("metadata.json", JSON.stringify({ packages: [parent, child], workspace_members: [], resolve: { nodes: [parent, child].map(p => ({ id: p.id })) } }));
  const extra = write("extra-license.txt", "Synthetic ParsecVDD attribution fixture.\n");
  const overrides = write("overrides.json", "{}");
  const output = path.join(dir, "NOTICES.md");
  return { dir, write, metadata, extra, overrides, output, run() { return spawnSync(process.execPath, [generator, "--metadata", metadata, "--output", output, "--extra-license", extra, "--overrides", overrides], { encoding: "utf8" }); } };
}

test("offline notices preserve attribution, license directories, and exact-commit workspace provenance deterministically", () => {
  const f = fixture();
  try {
    const first = f.run(); assert.equal(first.status, 0, first.stderr);
    const content = fs.readFileSync(f.output, "utf8");
    assert.match(content, /Copyright 2000 Original Fixture Author/);
    assert.match(content, /LICENSES\/MIT.txt/);
    assert.match(content, /NOTICE via upstream@1\.0\.0/);
    assert.match(content, /upstream a{40}/);
    assert.equal(content.includes(f.dir), false, "no local machine paths in public output");
    const second = f.run(); assert.equal(second.status, 0, second.stderr);
    assert.equal(fs.readFileSync(f.output, "utf8"), content);
    assert.deepEqual(JSON.parse(first.stdout), JSON.parse(second.stdout));
  } finally { removeFixture(f.dir); }
});

test("missing license text or mismatched workspace commit fails before replacing an existing notice", () => {
  const f = fixture();
  try {
    fs.writeFileSync(f.output, "previous valid notice");
    f.write("child/.cargo_vcs_info.json", JSON.stringify({ git: { sha1: "b".repeat(40) }, path_in_vcs: "child" }));
    const result = f.run(); assert.notEqual(result.status, 0);
    assert.match(result.stderr, /child@1\.0\.0: no license\/notice text found/);
    assert.equal(fs.readFileSync(f.output, "utf8"), "previous valid notice");
  } finally { removeFixture(f.dir); }
});

test("explicit source overrides require the declared upstream commit and original file hash", () => {
  const f = fixture();
  try {
    f.write("child/.cargo_vcs_info.json", JSON.stringify({ git: { sha1: "b".repeat(40) }, path_in_vcs: "child" }));
    const original = "Synthetic original license\nCopyright 2001 Explicit Author\n";
    f.write("child-original.txt", original);
    f.write("overrides.json", JSON.stringify({ "child@1.0.0": {
      repository: "https://example.org/public-library", commit: "b".repeat(40),
      files: [{ file: "child-original.txt", origin: "https://example.org/public-library/exact-commit/LICENSE", sha256: hash(original) }],
    } }));
    assert.equal(f.run().status, 0);
    assert.match(fs.readFileSync(f.output, "utf8"), /Copyright 2001 Explicit Author/);
    f.write("child-original.txt", "tampered text");
    const result = f.run(); assert.notEqual(result.status, 0); assert.match(result.stderr, /override hash mismatch/);
  } finally { removeFixture(f.dir); }
});

test("personal build paths inside notice source fail closed", () => {
  const f = fixture();
  try {
    f.write("upstream/NOTICE", "C:\\Users\\Admin\\private-build-path");
    const result = f.run(); assert.notEqual(result.status, 0); assert.match(result.stderr, /Personal build path/);
    assert.equal(fs.existsSync(f.output), false);
  } finally { removeFixture(f.dir); }
});
