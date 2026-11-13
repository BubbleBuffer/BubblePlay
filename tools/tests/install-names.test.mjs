import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "../..");
const { values } = JSON.parse(readFileSync(join(root, "fixtures/install-names-contract.json"), "utf8"));

// Names the install scripts share with the Rust host. The Rust side is checked by unit tests that read the
// same fixture; here each script must define the value, as the named variable or as a quoted literal.
test("install scripts use the names in fixtures/install-names-contract.json", () => {
  for (const [name, { value, ps }] of Object.entries(values)) {
    for (const { file, variable } of ps) {
      const text = readFileSync(join(root, file), "utf8");
      if (variable) {
        const match = new RegExp(`^\\$${variable}\\s*=\\s*'([^']*)'`, "m").exec(text);
        assert.equal(match?.[1], value, `${file} $${variable} must equal fixture "${name}"`);
      } else {
        assert.ok(text.includes(`'${value}'`), `${file} must mention '${value}' (fixture "${name}")`);
      }
    }
  }
});
