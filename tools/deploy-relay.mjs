// Build and deploy the relay Worker as one step.
//
//   node tools/deploy-relay.mjs --config <wrangler.prod.toml>            build, verify, wrangler dry run
//   node tools/deploy-relay.mjs --config <wrangler.prod.toml> --deploy   the same, then deploy
//   node tools/deploy-relay.mjs --config <file> --skip-build             use the bundle in relay-rs/worker/build
//                                                                        (for example one CI built), after checking its stamp
//   node tools/deploy-relay.mjs --config <file> --plan                   print the steps and run nothing
//
// The config (untracked wrangler.prod.toml) is an input: it holds the account, routes and secrets bindings.
// Rules: a clean tree, a bundle stamped with the commit being deployed, tool versions read from the
// places CI pins them (relay-rs/worker/Cargo.toml for worker-build, .github/workflows/checks.yml for wrangler).
import { existsSync, mkdtempSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const WORKER_DIR = join(ROOT, "relay-rs/worker");
const BUNDLE_DIR = join(WORKER_DIR, "build");
const BUNDLE_ENTRY = join(BUNDLE_DIR, "worker/shim.mjs");
const STAMP_FILE = join(BUNDLE_DIR, "BUILD_INFO.json");
// The paths whose content is the deployed relay: the bundle's sources and the viewer assets.
const DEPLOYED_PATHS = ["relay-rs", "crates", "worker/public"];
const COMMIT_VAR = "BUILD_COMMIT";
const MAX_CAPTURE_BYTES = 64 * 1024 * 1024;

export function pinnedWorkerBuildVersion(cargoToml) {
  const match = /^worker\s*=\s*"=(\d+\.\d+\.\d+)"/m.exec(cargoToml);
  if (!match) throw new Error("relay-rs/worker/Cargo.toml does not pin worker = \"=x.y.z\"");
  return match[1];
}

export function pinnedWranglerVersion(checksYml) {
  const match = /wrangler@(\d+\.\d+\.\d+)/.exec(checksYml);
  if (!match) throw new Error(".github/workflows/checks.yml does not pin wrangler@x.y.z");
  return match[1];
}

// Why a bundle must not be deployed, or null. `stamp` is the parsed BUILD_INFO.json or null.
export function bundleProblem(stamp, headCommit, bundleBuiltMs, headCommitMs) {
  if (!stamp) return "the bundle has no BUILD_INFO.json: build it with this script";
  if (stamp.commit !== headCommit) return `the bundle was built from ${stamp.commit}, HEAD is ${headCommit}`;
  if (bundleBuiltMs < headCommitMs) return "the bundle is older than the HEAD commit";
  return null;
}

function parseArgs(argv) {
  const options = { config: null, deploy: false, skipBuild: false, plan: false };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--config") options.config = argv[++i];
    else if (argv[i] === "--deploy") options.deploy = true;
    else if (argv[i] === "--skip-build") options.skipBuild = true;
    else if (argv[i] === "--plan") options.plan = true;
    else throw new Error(`unknown argument ${argv[i]}`);
  }
  if (!options.config) throw new Error("--config <wrangler.prod.toml> is required");
  options.config = resolve(options.config);
  if (!existsSync(options.config)) throw new Error(`config not found: ${options.config}`);
  return options;
}

function run(command, args, cwd, plan) {
  console.log(`$ ${[command, ...args].join(" ")}   (in ${cwd})`);
  if (plan) return "";
  const result = spawnSync(command, args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "inherit"], maxBuffer: MAX_CAPTURE_BYTES, shell: process.platform === "win32" });
  if (result.status !== 0) throw new Error(`${command} exited with ${result.status}`);
  return result.stdout;
}
const git = (...args) => spawnSync("git", args, { cwd: ROOT, encoding: "utf8" }).stdout.trim();

function main() {
  const options = parseArgs(process.argv.slice(2));
  const head = git("rev-parse", "HEAD");
  const headCommitMs = Number(git("show", "-s", "--format=%ct", "HEAD")) * 1000;
  const dirty = git("status", "--porcelain", "--untracked-files=no", "--", ...DEPLOYED_PATHS);
  if (dirty) throw new Error(`uncommitted changes in the deployed paths:\n${dirty}`);
  const workerBuild = pinnedWorkerBuildVersion(readFileSync(join(WORKER_DIR, "Cargo.toml"), "utf8"));
  const wrangler = pinnedWranglerVersion(readFileSync(join(ROOT, ".github/workflows/checks.yml"), "utf8"));
  console.log(`commit ${head}, worker-build ${workerBuild}, wrangler ${wrangler}`);

  if (!options.skipBuild) {
    const installed = options.plan ? workerBuild : (spawnSync("worker-build", ["--version"], { encoding: "utf8", shell: process.platform === "win32" }).stdout ?? "").trim().split(/\s+/).pop();
    if (installed !== workerBuild) throw new Error(`worker-build ${workerBuild} is required, found ${installed || "none"}: cargo install worker-build --version ${workerBuild} --locked`);
    run("worker-build", ["--release"], WORKER_DIR, options.plan);
    if (!options.plan) {
      writeFileSync(STAMP_FILE, `${JSON.stringify({ commit: head, builtAt: new Date().toISOString(), workerBuild }, null, 2)}\n`);
    }
  }
  if (!options.plan) {
    if (!existsSync(BUNDLE_ENTRY)) throw new Error(`no bundle at ${BUNDLE_ENTRY}`);
    const stamp = existsSync(STAMP_FILE) ? JSON.parse(readFileSync(STAMP_FILE, "utf8")) : null;
    const problem = bundleProblem(stamp, head, statSync(BUNDLE_ENTRY).mtimeMs, headCommitMs);
    if (problem) throw new Error(`refusing to deploy: ${problem}`);
  }

  const args = ["--yes", `wrangler@${wrangler}`, "deploy", "--config", options.config, "--var", `${COMMIT_VAR}:${head}`, "--message", head];
  if (options.deploy) {
    run("npx", args, WORKER_DIR, options.plan);
  } else {
    const outdir = mkdtempSync(join(tmpdir(), "relay-deploy-"));
    run("npx", [...args, "--dry-run", "--outdir", outdir], WORKER_DIR, options.plan);
    console.log("Dry run only. Pass --deploy to publish.");
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { main(); } catch (error) { console.error(error.message); process.exit(1); }
}
