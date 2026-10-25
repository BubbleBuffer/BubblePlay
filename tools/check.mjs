import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

// Run from any directory, without relying on a globally installed npm launcher.
const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const profile = process.argv[2] ?? "full";
if (!["full", "client", "host", "syntax"].includes(profile)) {
  console.error("Usage: node tools/check.mjs [full|client|host|syntax]");
  process.exit(2);
}
function run(command, args, options = {}) {
  const result = spawnSync(command, args, { cwd: root, stdio: "inherit", windowsHide: true, ...options });
  if (result.error) console.error(result.error.message);
  if (result.status !== 0) process.exit(result.status ?? 1);
}
function files(directory) {
  return readdirSync(directory, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))
    .flatMap(entry => entry.isDirectory() ? files(join(directory, entry.name)) : [join(directory, entry.name)]);
}
if (["full", "client", "syntax"].includes(profile)) {
  let checked = 0;
  for (const directory of ["worker/public", "worker/tools", "worker/test", "tools"]) {
    for (const file of files(join(root, directory))) {
      if (/\.(?:js|mjs)$/.test(file)) {
        run(process.execPath, ["--check", file]);
        checked++;
      } else if (file.endsWith(".html")) {
        for (const match of readFileSync(file, "utf8").matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script\s*>/gi)) {
          if (/\bsrc\s*=/i.test(match[1])) continue;
          const type = /\btype\s*=\s*["']([^"']+)["']/i.exec(match[1])?.[1];
          if (type && !["module", "text/javascript", "application/javascript"].includes(type)) continue;
          console.log(`Checking inline script: ${file}`);
          run(process.execPath, ["--check", `--input-type=${type === "module" ? "module" : "commonjs"}`],
            { input: match[2], stdio: ["pipe", "inherit", "inherit"] });
          checked++;
        }
      }
    }
  }
  console.log(`Syntax passed: ${checked} scripts.`);
  if (profile !== "syntax") run(process.execPath, ["--test", ...files(join(root, "worker/test")).filter(file => file.endsWith(".test.mjs"))]);
}
if (["full", "host"].includes(profile)) {
  const env = { ...process.env, NVIDIA_VIDEO_CODEC_SDK_PATH:
    process.env.NVIDIA_VIDEO_CODEC_SDK_PATH || join(root, "host/sdk-stub/") };
  run("cargo", ["test", "--locked", "--offline", "--manifest-path", "host/Cargo.toml", "--all-targets"], { env });
  run("cargo", ["check", "--locked", "--offline", "--manifest-path", "host/Cargo.toml", "--all-targets"], { env });
}
if (profile === "full") {
  run("git", ["-c", `safe.directory=${root.replaceAll("\\", "/")}`, "diff", "--check"]);
}
console.log(`BubblePlay ${profile} checks passed.`);
