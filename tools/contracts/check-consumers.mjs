// Rule 2 gate: every fixtures/*.json has a consumer test in each implementation that speaks it.
// Usage: node tools/contracts/check-consumers.mjs   (exit 1 on any violation)
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const config = JSON.parse(readFileSync(join(root, "tools/contracts/consumers.json"), "utf8"));
const SKIP_DIRS = new Set(["target", "node_modules", "build", ".gradle"]);
const SOURCE = /\.(?:mjs|js|kt|rs)$/;

function sources(directory) {
  if (!existsSync(directory)) return [];
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    if (entry.isDirectory()) return SKIP_DIRS.has(entry.name) ? [] : sources(join(directory, entry.name));
    return SOURCE.test(entry.name) ? [join(directory, entry.name)] : [];
  });
}

const texts = Object.fromEntries(Object.entries(config.roots).map(([group, dirs]) => [
  group,
  dirs.flatMap(dir => sources(join(root, dir))).map(file => readFileSync(file, "utf8")),
]));
const has = (group, name) => texts[group].some(text => text.includes(name));

const problems = [];
const onDisk = readdirSync(join(root, "fixtures")).filter(name => name.endsWith(".json")).sort();
for (const name of onDisk) {
  if (!config.fixtures[name]) problems.push(`${name}: no entry in tools/contracts/consumers.json`);
}
for (const [name, entry] of Object.entries(config.fixtures)) {
  if (!onDisk.includes(name)) problems.push(`${name}: listed but not in fixtures/`);
  const gaps = entry.gaps ?? {};
  for (const group of entry.consumers) {
    const found = has(group, name);
    if (!found && !gaps[group]) problems.push(`${name}: no ${group} test names it`);
    if (found && gaps[group]) problems.push(`${name}: ${group} now has a consumer, remove the gap`);
  }
  for (const group of Object.keys(gaps)) {
    if (!entry.consumers.includes(group)) problems.push(`${name}: gap for ${group}, which is not a consumer`);
  }
}

if (problems.length) {
  console.error(problems.join("\n"));
  process.exit(1);
}
console.log(`${onDisk.length} fixtures, all consumers present`);
