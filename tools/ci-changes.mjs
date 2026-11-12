// Decides which Checks jobs a change needs: `node tools/ci-changes.mjs [<base-branch>]`.
// With a base branch the changed files are `origin/<base>...HEAD`; without one (master pushes, manual
// runs) every job runs. Writes `<job>=true|false` lines to $GITHUB_OUTPUT (and stdout).
import { appendFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

// Job -> path prefixes (or exact files) that make it necessary.
export const JOB_PATHS = {
  relay: ["relay-rs/", "crates/", "fixtures/"],
  browser: ["worker/", "fixtures/"],
  e2e: ["worker/", "relay-rs/", "crates/", "fixtures/"],
  host: ["host/", "crates/", "fixtures/"],
  client: ["client-win/", "crates/", "fixtures/"],
  android: ["android/", "app/", "fixtures/"],
  packaging: ["host/", "client-win/", "app/", "branding/", "docs/", "LICENSE", "README.md"],
};
// A change to the CI definition or the tools the jobs run re-runs everything.
export const EVERYTHING_PATHS = [".github/", "tools/"];

const startsWithAny = (file, prefixes) => prefixes.some(prefix => file === prefix || file.startsWith(prefix));

export function decide(files) {
  const all = files === null || files.some(file => startsWithAny(file, EVERYTHING_PATHS));
  return Object.fromEntries(Object.entries(JOB_PATHS).map(([job, prefixes]) =>
    [job, all || files.some(file => startsWithAny(file, prefixes))]));
}

function changedFiles(base) {
  if (!base) return null;
  const result = spawnSync("git", ["diff", "--name-only", `origin/${base}...HEAD`], { encoding: "utf8" });
  if (result.status !== 0) throw new Error(`git diff against origin/${base} failed: ${result.stderr}`);
  return result.stdout.split("\n").filter(Boolean);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const decision = decide(changedFiles(process.argv[2]));
  const lines = Object.entries(decision).map(([job, run]) => `${job}=${run}`);
  console.log(lines.join("\n"));
  if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `${lines.join("\n")}\n`);
}
