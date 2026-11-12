// Lint gates, shrink-only: `node tools/lint/gates.mjs [gate ...] [--update | --init]`.
//
// One ratchet mechanism for every gate. A baseline file under tools/lint/ records today's violations as
// counts. A gate fails when any count grows, when a violation appears that is not in the baseline, and
// when a count shrank without the baseline being lowered, so the baseline can only tighten.
//   --update  lowers the baselines to the current counts (refuses to raise anything)
//   --init    writes the current counts unconditionally (new gate, or an owner-approved reset)
//
// Gates without arguments (cheap, seconds, no toolchain): length, suppress, eslint.
// Gates that need a toolchain, named explicitly:
//   clippy <project> [-- <extra cargo args>]   projects: see CLIPPY_PROJECTS
//   dart                                       needs `dart` on PATH and `flutter pub get` done in app/
//   detekt                                     needs DETEKT_CLI_JAR pointing at the detekt-cli jar
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const LINT_DIR = join(ROOT, "tools/lint");

export const MAX_FILE_LINES = 500;
const MAX_CAPTURE_BYTES = 512 * 1024 * 1024;

// Directories under the gates. `app` is only lib/ and android/ (the Rust bridge has its own checks).
const COMPONENT_DIRS = {
  host: ["host"],
  "relay-rs": ["relay-rs"],
  crates: ["crates"],
  "client-win": ["client-win"],
  worker: ["worker"],
  app: ["app/lib", "app/android"],
  tools: ["tools"],
};
const CODE_EXTENSIONS = /\.(?:rs|js|mjs|ts|dart|kt|kts|java|ps1|py)$/;
// Generated and vendored files, by path. Everything else is ours and is counted.
const EXCLUDED_PATHS = [
  /^app\/lib\/src\/rust\/frb_generated[^/]*\.dart$/,
  /\.(?:g|freezed)\.dart$/,
  /^worker\/public\/vendor\//,
  /^app\/android\/.*\/GeneratedPluginRegistrant\.(?:java|kt)$/,
];
// The gate's own sources and baselines spell the suppression patterns out.
const SUPPRESSION_SCAN_SKIP = /^tools\/lint\/|^tools\/tests\/lint-gates\.test\.mjs$/;

export const SUPPRESSION_PATTERNS = {
  "#[allow(": /#!?\[allow\(/g,
  "eslint-disable": /eslint-disable/g,
  "// ignore:": /\/\/\s*ignore(?:_for_file)?:/g,
  "@Suppress": /@Suppress\b/g,
  "empty catch": /\bcatch\s*(?:\([^)]*\))?\s*\{\s*\}/g,
};

// Clippy lints on top of `-D warnings` in the existing jobs. Thresholds live in tools/lint/clippy.toml.
export const CLIPPY_RATCHET_LINTS = [
  "unwrap_used", "expect_used", "too_many_lines", "cognitive_complexity", "let_underscore_must_use",
];
const CLIPPY_PROJECTS = {
  host: { dir: "host", args: [], skip: [] },
  "client-win": { dir: "client-win", args: [], skip: [] },
  "relay-rs": { dir: "relay-rs", args: ["--workspace"], skip: ["relay-rs/worker/"] },
  "relay-worker": { dir: "relay-rs/worker", args: ["--target", "wasm32-unknown-unknown"], skip: [] },
  "bp-device-key": { dir: "crates/bp-device-key", args: ["--all-features"], skip: [] },
};
// max-lines is the length gate's job (one place for the 500 limit).
const ESLINT_SKIPPED_RULES = new Set(["max-lines"]);

const trackedFiles = () => spawnSync("git", ["ls-files", "-z"], { cwd: ROOT, encoding: "utf8", maxBuffer: MAX_CAPTURE_BYTES })
  .stdout.split("\0").filter(Boolean);

export function componentOf(path) {
  for (const [name, dirs] of Object.entries(COMPONENT_DIRS)) {
    if (dirs.some(dir => path === dir || path.startsWith(`${dir}/`))) return name;
  }
  return null;
}

export function sourceFiles(list = trackedFiles()) {
  return list.filter(path => componentOf(path) && CODE_EXTENSIONS.test(path)
    && !EXCLUDED_PATHS.some(rule => rule.test(path)) && existsSync(join(ROOT, path)));
}

export function countLines(text) {
  if (text.length === 0) return 0;
  let lines = 1;
  for (let i = 0; i < text.length; i++) if (text.charCodeAt(i) === 10) lines++;
  return text.endsWith("\n") ? lines - 1 : lines;
}

// ---- ratchet -------------------------------------------------------------------------------------

const KEY_SEPARATOR = "\u0000";
export const flatten = map => Object.fromEntries(Object.entries(map).flatMap(([key, value]) =>
  typeof value === "number" ? [[key, value]]
    : Object.entries(value).map(([rule, count]) => [`${key}${KEY_SEPARATOR}${rule}`, count])));
export function nest(flat) {
  const out = {};
  for (const key of Object.keys(flat).sort()) {
    const [first, rule] = key.split(KEY_SEPARATOR);
    if (rule === undefined) out[first] = flat[key];
    else (out[first] ??= {})[rule] = flat[key];
  }
  return out;
}
const label = key => key.replace(KEY_SEPARATOR, "  ");

// Compares flat count maps. `current` holds only violations (zero counts are absent).
export function compare(current, baseline) {
  const grew = [];
  const stale = [];
  for (const [key, count] of Object.entries(current)) {
    const allowed = baseline[key] ?? 0;
    if (count > allowed) grew.push({ key, count, allowed });
    else if (count < allowed) stale.push({ key, count, allowed });
  }
  for (const [key, allowed] of Object.entries(baseline)) {
    if (!(key in current)) stale.push({ key, count: 0, allowed });
  }
  return { grew, stale };
}

// `inScope` limits the comparison to the part of a shared baseline this run measured.
export function ratchet(gate, current, file, mode, inScope = () => true) {
  const path = join(LINT_DIR, file);
  const stored = existsSync(path) ? JSON.parse(readFileSync(path, "utf8")) : {};
  const flatStored = flatten(stored);
  const baseline = Object.fromEntries(Object.entries(flatStored).filter(([key]) => inScope(key)));
  const { grew, stale } = compare(current, baseline);
  const write = () => {
    const kept = Object.fromEntries(Object.entries(flatStored).filter(([key]) => !inScope(key)));
    writeFileSync(path, `${JSON.stringify(nest({ ...kept, ...current }), null, 2)}\n`);
  };
  if (mode === "init") { write(); console.log(`${gate}: wrote ${file}`); return true; }
  if (mode === "update" && grew.length === 0) {
    if (stale.length) { write(); console.log(`${gate}: lowered ${stale.length} entries in ${file}`); }
    else console.log(`${gate}: ${file} already matches`);
    return true;
  }
  for (const { key, count, allowed } of grew) {
    console.error(`${gate}: ${label(key)} is ${count}, allowed ${allowed}. Fix it; the baseline never goes up.`);
  }
  if (mode !== "update") {
    for (const { key, count, allowed } of stale) {
      console.error(`${gate}: ${label(key)} is ${count}, baseline says ${allowed}. Lower it: node tools/lint/gates.mjs ${gate.split(" ")[0]} --update`);
    }
  }
  const ok = grew.length === 0 && stale.length === 0;
  if (ok) console.log(`${gate}: ok (${Object.keys(baseline).length} baselined entries)`);
  return ok;
}

// ---- cheap gates ---------------------------------------------------------------------------------

export function measureLength(files, read) {
  const over = {};
  for (const path of files) {
    const lines = countLines(read(path));
    if (lines > MAX_FILE_LINES) over[path] = lines;
  }
  return over;
}

export function measureSuppressions(files, read) {
  const counts = {};
  for (const path of files) {
    if (SUPPRESSION_SCAN_SKIP.test(path)) continue;
    const text = read(path);
    for (const [name, pattern] of Object.entries(SUPPRESSION_PATTERNS)) {
      const found = text.match(pattern)?.length ?? 0;
      if (found) {
        const key = `${componentOf(path)}${KEY_SEPARATOR}${name}`;
        counts[key] = (counts[key] ?? 0) + found;
      }
    }
  }
  return counts;
}

const readText = path => readFileSync(join(ROOT, path), "utf8");

// ---- toolchain gates -----------------------------------------------------------------------------

// Each parser takes a tool's raw output and returns a flat map keyed `file<sep>rule`.
export function addCount(map, file, rule) {
  const key = `${file}${KEY_SEPARATOR}${rule}`;
  map[key] = (map[key] ?? 0) + 1;
}
const repoPath = (cwd, file) => relative(ROOT, resolve(cwd, file)).replaceAll("\\", "/");

export function parseClippy(stdout, project, cwd) {
  const counts = {};
  const seen = new Set();
  const wanted = new Set(CLIPPY_RATCHET_LINTS.map(name => `clippy::${name}`));
  for (const line of stdout.split("\n")) {
    if (!line.startsWith("{")) continue;
    let message;
    try { message = JSON.parse(line).message; } catch { continue; }
    const code = message?.code?.code;
    if (!wanted.has(code)) continue;
    const span = message.spans.find(candidate => candidate.is_primary);
    if (!span) continue;
    const file = repoPath(cwd, span.file_name);
    if (!file.startsWith(`${project.dir}/`) || project.skip.some(prefix => file.startsWith(prefix))) continue;
    // The same source is compiled for the lib and its test target: count each location once.
    const where = `${file}:${span.line_start}:${span.column_start}:${code}`;
    if (seen.has(where)) continue;
    seen.add(where);
    addCount(counts, file, code);
  }
  return counts;
}

// `dart analyze --format=machine`: SEVERITY|TYPE|CODE|FILE|LINE|COLUMN|LENGTH|MESSAGE
export function parseDart(output, cwd) {
  const counts = {};
  for (const line of output.split("\n")) {
    const fields = line.split("|");
    if (fields.length < 8 || !/^(?:INFO|WARNING|ERROR)$/.test(fields[0])) continue;
    addCount(counts, repoPath(cwd, fields[3]), fields[2].toLowerCase());
  }
  return counts;
}

// detekt `--report xml:<file>` (checkstyle): <file name="..."><error ... source="detekt.RuleName" />
export function parseDetekt(xml) {
  const counts = {};
  for (const block of xml.matchAll(/<file name="([^"]+)">([\s\S]*?)<\/file>/g)) {
    const file = repoPath(ROOT, block[1]);
    for (const error of block[2].matchAll(/source="detekt\.([^"]+)"/g)) addCount(counts, file, error[1]);
  }
  return counts;
}

function capture(command, args, cwd, env = process.env) {
  const result = spawnSync(command, args, { cwd, env, encoding: "utf8", maxBuffer: MAX_CAPTURE_BYTES, windowsHide: true });
  if (result.error) throw result.error;
  return result;
}

function runClippy(name, extraArgs) {
  const project = CLIPPY_PROJECTS[name];
  if (!project) throw new Error(`unknown clippy project "${name}"; known: ${Object.keys(CLIPPY_PROJECTS).join(", ")}`);
  const cwd = join(ROOT, project.dir);
  const lints = CLIPPY_RATCHET_LINTS.flatMap(lint => ["-W", `clippy::${lint}`]);
  const args = ["clippy", "--locked", "--all-targets", "--message-format=json", ...project.args, ...extraArgs, "--", ...lints];
  const result = capture("cargo", args, cwd, { ...process.env, CLIPPY_CONF_DIR: LINT_DIR });
  if (!result.stdout.includes('"reason":"build-finished"')) {
    console.error(result.stderr);
    throw new Error(`cargo clippy failed to run for ${name}`);
  }
  const prefix = `${project.dir}/`;
  const inScope = key => key.startsWith(prefix) && !project.skip.some(item => key.startsWith(item));
  return { counts: parseClippy(result.stdout, project, cwd), inScope };
}

function runEslint() {
  const cwd = join(ROOT, "worker");
  const bin = join(cwd, "node_modules/eslint/bin/eslint.js");
  if (!existsSync(bin)) throw new Error("ESLint is not installed: run `npm --prefix worker ci --ignore-scripts`");
  const result = capture(process.execPath, [bin, "--format", "json", "."], cwd);
  const counts = {};
  for (const report of JSON.parse(result.stdout)) {
    for (const message of report.messages) {
      const rule = message.ruleId ?? "parse-error";
      if (!ESLINT_SKIPPED_RULES.has(rule)) addCount(counts, repoPath(cwd, report.filePath), rule);
    }
  }
  return counts;
}

function runDart() {
  const cwd = join(ROOT, "app");
  const result = capture("dart", ["analyze", "--format=machine", "."], cwd);
  return parseDart(`${result.stdout}\n${result.stderr}`, cwd);
}

function runDetekt() {
  const jar = process.env.DETEKT_CLI_JAR;
  if (!jar) throw new Error("set DETEKT_CLI_JAR to the detekt-cli jar");
  const report = join(ROOT, "tools/lint/.detekt-report.xml");
  capture("java", ["-jar", jar, "--input", "android/app/src,app/android/app/src", "--config", "tools/lint/detekt.yml",
    "--build-upon-default-config", "--report", `xml:${report}`], ROOT);
  return parseDetekt(readFileSync(report, "utf8"));
}

// ---- main ----------------------------------------------------------------------------------------

function main(argv) {
  const mode = argv.includes("--update") ? "update" : argv.includes("--init") ? "init" : "check";
  const separator = argv.indexOf("--");
  const extraArgs = separator === -1 ? [] : argv.slice(separator + 1);
  const words = (separator === -1 ? argv : argv.slice(0, separator)).filter(word => !word.startsWith("--"));
  const gates = words.length ? words : ["length", "suppress", "eslint"];
  let ok = true;
  for (let i = 0; i < gates.length; i++) {
    const gate = gates[i];
    if (gate === "length") {
      ok = ratchet("length", measureLength(sourceFiles(), readText), "file-length-allowlist.json", mode) && ok;
    } else if (gate === "suppress") {
      ok = ratchet("suppress", measureSuppressions(sourceFiles(), readText), "suppression-baseline.json", mode) && ok;
    } else if (gate === "eslint") {
      ok = ratchet("eslint", runEslint(), "eslint-baseline.json", mode) && ok;
    } else if (gate === "clippy") {
      const { counts, inScope } = runClippy(gates[++i], extraArgs);
      ok = ratchet(`clippy ${gates[i]}`, counts, "clippy-baseline.json", mode, inScope) && ok;
    } else if (gate === "dart") {
      ok = ratchet("dart", runDart(), "dart-baseline.json", mode) && ok;
    } else if (gate === "detekt") {
      ok = ratchet("detekt", runDetekt(), "detekt-baseline.json", mode) && ok;
    } else {
      throw new Error(`unknown gate "${gate}"`);
    }
  }
  process.exit(ok ? 0 : 1);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main(process.argv.slice(2));
