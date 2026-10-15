#!/usr/bin/env node
// Offline notice generation from an existing Cargo metadata snapshot.
import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
import { fileURLToPath } from "node:url";

const projectRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2), options = {};
for (let i = 0; i < args.length; i++) {
  if (!["--metadata", "--output", "--extra-license", "--overrides"].includes(args[i]) || !args[i + 1]) throw new Error("Usage: node tools/generate-notices.mjs --metadata FILE --output FILE [--extra-license ParsecVDD-LICENSE] [--overrides FILE]");
  options[args[i].slice(2)] = args[++i];
}
if (!options.metadata || !options.output) throw new Error("Both --metadata and --output are required; this tool never downloads dependencies.");

const order = (a, b) => a < b ? -1 : a > b ? 1 : 0;
const sha256 = bytes => crypto.createHash("sha256").update(bytes).digest("hex");
const decode = bytes => new TextDecoder("utf-8", { fatal: true }).decode(bytes);
const markdown = text => String(text).replaceAll("|", "\\|").replaceAll("\n", " ").replaceAll("\r", "");
const metadata = JSON.parse(decode(fs.readFileSync(options.metadata)).replace(/^\uFEFF/, ""));
if (!Array.isArray(metadata.packages) || !metadata.resolve?.nodes || !Array.isArray(metadata.workspace_members)) throw new Error("Cargo metadata with its resolved dependency graph is required.");
const resolved = new Set(metadata.resolve.nodes.map(node => node.id));
const workspace = new Set(metadata.workspace_members);
const packages = metadata.packages.filter(pkg => resolved.has(pkg.id) && !workspace.has(pkg.id))
  .sort((a, b) => order(`${a.name}@${a.version}`, `${b.name}@${b.version}`));
const blobs = new Map(), inventory = [], missing = [];
const overrideFile = options.overrides || path.join(projectRoot, "tools", "third-party", "notice-overrides.json");
const overrides = fs.existsSync(overrideFile) ? JSON.parse(decode(fs.readFileSync(overrideFile))) : {};
const repository = pkg => (pkg.repository || "").replace(/\.git$/, "");
const licenseExpression = pkg => (pkg.license || "").replaceAll("/", " OR ").replace(/\s+/g, "");
function vcs(pkg) {
  const file = path.join(path.dirname(pkg.manifest_path), ".cargo_vcs_info.json");
  return fs.existsSync(file) ? JSON.parse(decode(fs.readFileSync(file))) : null;
}

function assertRegular(file) {
  if (!fs.lstatSync(file).isFile() || fs.lstatSync(file).isSymbolicLink()) throw new Error(`Notice source is not a regular file: ${file}`);
}
function licenseFiles(directory) {
  const files = [];
  const visit = (dir, relative = "", insideLicenses = false) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => order(a.name, b.name))) {
      const full = path.join(dir, entry.name), rel = path.posix.join(relative, entry.name);
      if (entry.isSymbolicLink()) throw new Error(`Symbolic notice source is not accepted: ${full}`);
      if (entry.isDirectory()) {
        if (![".git", "target", "node_modules"].includes(entry.name)) visit(full, rel, insideLicenses || /^licen[cs]es$/i.test(entry.name));
      } else if (insideLicenses || /^(?:licen[cs]e|notice|copying|copyright)(?:[._-].*)?$/i.test(entry.name) || /(?:^|[._-])licen[cs]e(?:\.[^.]+)?$/i.test(entry.name)) files.push(rel);
    }
  };
  visit(directory);
  return files;
}
function readNoticeText(file, origin) {
  assertRegular(file);
  const bytes = fs.readFileSync(file);
  if (!bytes.length || bytes.length > 8 * 1024 * 1024) throw new Error(`Empty or oversized notice source: ${origin}`);
  const text = decode(bytes);
  // License authors' published names/contact details are retained. A local
  // machine path must never leak from build metadata into the public notice.
  if (/C:[\\/]+Users[\\/]+Admin/i.test(text) || text.includes(projectRoot) || text.includes(projectRoot.replaceAll("\\", "/"))) throw new Error(`Personal build path found inside notice text: ${origin}`);
  return { bytes, text };
}
function addText(file, origin) {
  const { bytes, text } = readNoticeText(file, origin);
  const hash = sha256(bytes);
  if (!blobs.has(hash)) blobs.set(hash, { text, origins: [] });
  blobs.get(hash).origins.push(origin);
  return hash;
}

for (const pkg of packages) {
  const label = `${pkg.name}@${pkg.version}`;
  const dir = path.dirname(pkg.manifest_path);
  if (!pkg.license && !pkg.license_file) { missing.push(`${label}: no license declaration`); continue; }
  const files = licenseFiles(dir);
  if (pkg.license_file) {
    const declared = path.resolve(dir, pkg.license_file);
    if (!declared.startsWith(dir + path.sep)) throw new Error(`License file escapes its crate: ${label}`);
    const relative = path.relative(dir, declared).replaceAll("\\", "/");
    if (!files.includes(relative)) files.push(relative);
  }
  files.sort(order);
  let texts = files.map(file => ({ file, hash: addText(path.join(dir, file), `${label}/${file}`) }));
  if (!texts.length) {
    // Cargo workspace child crates sometimes omit the root license. Reuse
    // only a root crate from the identical repository and VCS commit.
    const commit = vcs(pkg)?.git?.sha1;
    const parent = commit && packages.find(candidate => candidate.id !== pkg.id && repository(candidate) === repository(pkg)
      && licenseExpression(candidate) === licenseExpression(pkg)
      && vcs(candidate)?.git?.sha1 === commit && vcs(candidate)?.path_in_vcs === "" && licenseFiles(path.dirname(candidate.manifest_path)).length);
    if (parent) texts = licenseFiles(path.dirname(parent.manifest_path)).map(file => ({
      file: `${file} via ${parent.name}@${parent.version}`,
      hash: addText(path.join(path.dirname(parent.manifest_path), file), `${label}/${file} (workspace ${parent.name}@${parent.version}, upstream ${commit})`),
    }));
    else if (overrides[label]) {
      const override = overrides[label];
      if (override.repository !== pkg.repository || override.commit !== commit) throw new Error(`Notice override does not match upstream provenance: ${label}`);
      texts = override.files.map(source => {
        const full = path.resolve(path.dirname(overrideFile), source.file);
        if (!full.startsWith(path.dirname(path.resolve(overrideFile)) + path.sep)) throw new Error(`Notice override escapes its directory: ${label}`);
        assertRegular(full);
        if (sha256(fs.readFileSync(full)) !== source.sha256) throw new Error(`Notice override hash mismatch: ${label}/${source.file}`);
        return { file: source.file, hash: addText(full, `${label}: ${source.origin}`) };
      });
    }
  }
  if (!texts.length) { missing.push(`${label}: no license/notice text found`); continue; }
  inventory.push({ label, license: pkg.license || "See declared license file", texts });
}
if (missing.length) throw new Error("Missing dependency notice sources; no notice was written:\n" + missing.join("\n"));

const extra = options["extra-license"] || path.join(projectRoot, "tools", "third-party", "parsec-vdd-LICENSE.txt");
const extraHash = addText(extra, "adapted ParsecVDD / original MIT license");
const hashes = [...blobs.keys()].sort(order);
const ids = new Map(hashes.map((hash, i) => [hash, i + 1]));
// Hand-kept components outside the Cargo graph. Their texts come from the
// repository files named here and get their own appended ids.
const nonCargo = [
  { name: "hash-wasm", version: "4.12.0", license: "MIT", source: "https://registry.npmjs.org/hash-wasm/-/hash-wasm-4.12.0.tgz", file: "worker/public/vendor/hash-wasm/LICENSE" },
  { name: "bcprov-jdk18on", version: "1.86", license: "MIT", source: "https://raw.githubusercontent.com/bcgit/bc-java/r1rv86/LICENSE.html", file: "android/licenses/bouncycastle-LICENSE.txt" },
];
const extraTexts = nonCargo.map((component, i) => {
  const { bytes, text } = readNoticeText(path.join(projectRoot, component.file), component.file);
  return { ...component, id: hashes.length + i + 1, hash: sha256(bytes), text };
});
let output = "# Third-party notices\n\n";
function appendText(id, hash, origins, text) {
  output += `<a id="license-text-${id}"></a>\n\n### License text ${id}\n\n`;
  output += `Original file SHA-256: \`${hash}\`.\n\nSources:\n\n`;
  for (const origin of origins) output += `- ${markdown(origin)}\n`;
  // A dynamically sized fence preserves texts that contain Markdown fences.
  const body = text.replaceAll("\r\n", "\n").replaceAll("\r", "\n").replace(/\n?$/, "\n");
  const runs = body.match(/`+/g) || [];
  const fence = "`".repeat(Math.max(3, ...runs.map(run => run.length + 1)));
  output += `\n${fence}text\n${body}${fence}\n\n`;
}
output += `This Windows release uses the following ${inventory.length} locked Rust dependency versions and adapted ParsecVDD code. `;
output += "The inventory covers the entire resolved Windows Cargo graph, including build dependencies; it is intentionally broader than the code linked into the executables.\n\n";
output += "The license expressions below are the upstream declarations. All discovered license, notice, copying and copyright files are retained in full; no alternative licensing choice is asserted. Identical source texts share an appendix entry.\n\n";
output += "## Dependency inventory\n\n| Package | Upstream license expression | Retained texts |\n| --- | --- | --- |\n";
for (const item of inventory) {
  const links = item.texts.map(source => `[${markdown(source.file)}](#license-text-${ids.get(source.hash)})`).join("; ");
  output += `| ${markdown(item.label)} | ${markdown(item.license)} | ${links} |\n`;
}
output += `\n## Adapted ParsecVDD code\n\nThe original MIT license and copyright attribution are retained in [license text ${ids.get(extraHash)}](#license-text-${ids.get(extraHash)}).\n\n`;
output += "## Retained license and notice texts\n\n";
for (const hash of hashes) {
  const blob = blobs.get(hash);
  appendText(ids.get(hash), hash, [...new Set(blob.origins)].sort(order), blob.text);
}
output += "## Additional non-Cargo components\n\n";
output += "These components sit outside the Cargo graph and are listed by hand. Each licence file is retained in full from the repository path shown; it keeps its own entry even where the text matches a Cargo appendix entry.\n\n";
output += "| Component | Version | Licence | Source | Repository licence file | Retained text |\n| --- | --- | --- | --- | --- | --- |\n";
for (const item of extraTexts) output += `| ${markdown(item.name)} | ${markdown(item.version)} | ${markdown(item.license)} | ${markdown(item.source)} | ${markdown(item.file)} | [license text ${item.id}](#license-text-${item.id}) |\n`;
output += "\n";
for (const item of extraTexts) appendText(item.id, item.hash, [`${item.name}@${item.version} source: ${item.source}`, `${item.name}@${item.version} repository licence file: ${item.file}`], item.text);
const target = path.resolve(options.output);
fs.mkdirSync(path.dirname(target), { recursive: true });
fs.writeFileSync(target, output, "utf8");
console.log(JSON.stringify({ packages: inventory.length, retainedFiles: inventory.reduce((n, p) => n + p.texts.length, 1), uniqueTexts: hashes.length, sha256: sha256(Buffer.from(output)) }));
