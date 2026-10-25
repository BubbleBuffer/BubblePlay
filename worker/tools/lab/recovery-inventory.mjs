import { createHash } from "node:crypto";
import { closeSync, openSync, readSync } from "node:fs";
import { mkdir, stat, writeFile } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { spawnSync } from "node:child_process";

import { STREAM_LAB_COMPATIBILITY } from "../../public/media.js";

export const RECOVERY_INVENTORY_SCHEMA = 1;
export const DEFAULT_FULL_HASH_LIMIT_BYTES = 1024 * 1024;
export const DEFAULT_SAMPLE_BYTES = 64 * 1024;
const MODULE_DIRECTORY = dirname(fileURLToPath(import.meta.url));

const SOURCE_PREFIXES = ["host/src/", "worker/public/"];
const GENERATED_PREFIXES = [
  "target/", "target-stage1/", "host/target-", "host/sdk-stub/",
  "node_modules/", "worker/node_modules/", "worker/.wrangler/", "worker/.wrangler-dry-run/",
];
const CONFIGURATION_NAMES = new Set([
  ".gitignore", "cargo.toml", "cargo.lock", "package.json", "package-lock.json",
  "wrangler.toml", "wrangler.json", "wrangler.jsonc",
]);

export function classifyWorkspacePath(path) {
  const normalized = String(path ?? "").replaceAll("\\", "/").replace(/^\.\//, "");
  const lower = normalized.toLowerCase();
  if (!lower) return "other";
  if (lower === "artifacts" || lower.startsWith("artifacts/") ||
      /^(?:host-run|relay-).+\.(?:log|txt)$/.test(lower) ||
      /\.(?:log|out|err)$/.test(lower)) return "evidence";
  if (GENERATED_PREFIXES.some(prefix => lower === prefix.slice(0, -1) || lower.startsWith(prefix)) ||
      /(^|\/)target-[^/]+\//.test(lower)) return "generated";
  if (lower.startsWith("host/scripts/") || lower.endsWith(".ps1")) return "deployment";
  if (/(^|\/)tests?\//.test(lower) || /\.test\.m?[jt]s$/.test(lower)) return "test";
  if (SOURCE_PREFIXES.some(prefix => lower.startsWith(prefix))) return "source";
  if (CONFIGURATION_NAMES.has(basename(lower)) || /\.(?:toml|lock|jsonc?|yaml|yml)$/.test(lower))
    return "configuration";
  if (/\.(?:md|txt)$/.test(lower)) return "documentation";
  return "other";
}

export function redactCommandLine(value) {
  let redacted = String(value ?? "");
  const names = "token|password|authorization|app-password|secret|bearer";
  redacted = redacted.replace(
    new RegExp(`(--(?:${names}))=(?:"[^"]*"|'[^']*'|\\S+)`, "gi"),
    "$1=[REDACTED]",
  );
  redacted = redacted.replace(
    new RegExp(`(--(?:${names}))\\s+(?:"[^"]*"|'[^']*'|Bearer\\s+\\S+|\\S+)`, "gi"),
    "$1 [REDACTED]",
  );
  redacted = redacted.replace(/(wss?:\/\/)[^@\s/]+@/gi, "$1[REDACTED]@");
  redacted = redacted.replace(/([?&](?:token|password|authorization|secret)=)[^&\s]+/gi, "$1[REDACTED]");
  return redacted;
}

export async function fingerprintFile(path, options = {}) {
  const metadata = await stat(path);
  if (!metadata.isFile()) throw new TypeError(`Cannot fingerprint non-file '${path}'.`);
  const fullHashLimitBytes = options.fullHashLimitBytes ?? DEFAULT_FULL_HASH_LIMIT_BYTES;
  const sampleBytes = Math.max(1, options.sampleBytes ?? DEFAULT_SAMPLE_BYTES);
  if (metadata.size <= fullHashLimitBytes) {
    const bytes = await import("node:fs/promises").then(module => module.readFile(path));
    return {
      bytes: metadata.size,
      modifiedAt: metadata.mtime.toISOString(),
      hashMode: "full",
      sha256: createHash("sha256").update(bytes).digest("hex"),
      sampledBytes: metadata.size,
    };
  }

  const length = Math.min(sampleBytes, metadata.size);
  const head = Buffer.alloc(length);
  const tail = Buffer.alloc(length);
  const handle = openSync(path, "r");
  try {
    readSync(handle, head, 0, length, 0);
    readSync(handle, tail, 0, length, Math.max(0, metadata.size - length));
  } finally {
    closeSync(handle);
  }
  const digest = createHash("sha256")
    .update(`bubbleplay-head-tail-size-v1\0${metadata.size}\0`)
    .update(head)
    .update(tail)
    .digest("hex");
  return {
    bytes: metadata.size,
    modifiedAt: metadata.mtime.toISOString(),
    hashMode: "head-tail-size",
    sha256: digest,
    sampledBytes: length * 2,
  };
}

export function summarizeWorkspaceEntries(entries) {
  const byClass = {};
  const hashModes = {};
  let bytes = 0;
  for (const entry of entries) {
    const preservationClass = entry.preservationClass ?? classifyWorkspacePath(entry.path);
    const size = Number(entry.bytes ?? 0);
    const bucket = byClass[preservationClass] ??= { files: 0, bytes: 0 };
    bucket.files++;
    bucket.bytes += size;
    bytes += size;
    const mode = entry.hashMode ?? "none";
    hashModes[mode] = (hashModes[mode] ?? 0) + 1;
  }
  return {
    files: entries.length,
    bytes,
    byClass: sortObject(byClass),
    hashModes: sortObject(hashModes),
  };
}

export async function captureRecoveryInventory(options = {}) {
  const root = resolve(options.root ?? join(MODULE_DIRECTORY, "..", "..", ".."));
  const run = options.commandRunner ?? runCommand;
  const capturedAt = options.capturedAt ?? new Date();
  const id = capturedAt.toISOString().replace(/[:.]/g, "-");
  const outputRoot = resolve(options.outputRoot ?? join(root, "artifacts", "recovery-inventory"));
  if (outputRoot !== resolve(join(root, "artifacts", "recovery-inventory")) &&
      !outputRoot.startsWith(`${resolve(join(root, "artifacts", "recovery-inventory"))}\\`))
    throw new RangeError("Recovery inventory output must remain under artifacts/recovery-inventory.");
  const destination = join(outputRoot, id);
  await mkdir(destination, { recursive: true });

  const safeDirectory = root.replaceAll("\\", "/");
  const git = (args, allowFailure = false) => run("git", ["-c", `safe.directory=${safeDirectory}`, ...args], { cwd: root, allowFailure });
  const branch = git(["branch", "--show-current"]).stdout.trim();
  const head = git(["rev-parse", "HEAD"]).stdout.trim();
  const tracked = lines(git(["diff", "--name-only"]).stdout);
  const untracked = lines(git(["ls-files", "--others", "--exclude-standard"]).stdout);
  const paths = [...new Set([...tracked, ...untracked])].sort();
  const entries = [];
  const errors = [];
  for (const path of paths) {
    const absolute = resolve(root, path);
    if (relative(root, absolute).startsWith("..") || isAbsolute(relative(root, absolute))) {
      errors.push({ path, code: "outside-root" });
      continue;
    }
    try {
      const preservationClass = classifyWorkspacePath(path);
      if (preservationClass === "generated") {
        const metadata = await stat(absolute);
        entries.push({ path, preservationClass, bytes: metadata.size, modifiedAt: metadata.mtime.toISOString(), hashMode: "aggregate" });
      } else {
        const fingerprint = await fingerprintFile(absolute, options);
        entries.push({ path, preservationClass, ...fingerprint });
      }
    } catch (error) {
      errors.push({ path, code: error.code ?? "inspect-failed", message: error.message });
    }
  }

  const services = ["BubblePlayBroker", "BubblePlayTransport", "BubblePlayLabHelper"].map(name => {
    const query = run("sc.exe", ["query", name], { cwd: root, allowFailure: true });
    const config = run("sc.exe", ["qc", name], { cwd: root, allowFailure: true });
    return { name, queryStatus: query.status, configStatus: config.status,
      query: redactCommandLine(query.stdout), config: redactCommandLine(config.stdout) };
  });
  const installed = [];
  for (const name of ["bubbleplay_broker.exe", "bubbleplay_transport.exe", "bubbleplay_agent.exe", "bubbleplay_input_proxy.exe", "bubbleplay_lab_helper.exe", "bubbleplay-host.exe"]) {
    const path = join(process.env.ProgramFiles ?? "C:\\Program Files", "BubblePlay", name);
    try { installed.push({ name, path, ...(await fingerprintFile(path, options)) }); }
    catch (error) { installed.push({ name, path, missing: error.code === "ENOENT", error: error.code ?? error.message }); }
  }

  const inventory = {
    schemaVersion: RECOVERY_INVENTORY_SCHEMA,
    capturedAt: capturedAt.toISOString(),
    classification: "unverified-checkpoint",
    root,
    git: { branch, head, trackedModified: tracked.length, untracked: untracked.length },
    compatibility: STREAM_LAB_COMPATIBILITY,
    workspace: { summary: summarizeWorkspaceEntries(entries), entries, errors },
    runtime: { services, installed },
  };
  const output = join(destination, "inventory.json");
  await writeFile(output, `${JSON.stringify(inventory, null, 2)}\n`, "utf8");
  return { output, inventory };
}

function runCommand(file, args, options = {}) {
  const result = spawnSync(file, args, { cwd: options.cwd, encoding: "utf8", windowsHide: true, timeout: 30_000 });
  const status = result.status ?? (result.error ? -1 : 0);
  if (status !== 0 && !options.allowFailure)
    throw new Error(`${file} failed with status ${status}: ${result.stderr || result.error?.message || "unknown error"}`);
  return { status, stdout: result.stdout ?? "", stderr: result.stderr ?? "" };
}

function lines(value) {
  return String(value ?? "").split(/\r?\n/).map(item => item.trim()).filter(Boolean);
}

function sortObject(value) {
  return Object.fromEntries(Object.entries(value).sort(([a], [b]) => a.localeCompare(b)));
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  captureRecoveryInventory({ root: resolve(process.argv[2] ?? join(dirname(process.argv[1]), "..", "..", "..")) })
    .then(({ output, inventory }) => process.stdout.write(`${JSON.stringify({
      result: "pass", output, summary: inventory.workspace.summary, git: inventory.git,
    }, null, 2)}\n`))
    .catch(error => { process.stderr.write(`${error.stack ?? error.message}\n`); process.exitCode = 1; });
}
