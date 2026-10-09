#!/usr/bin/env node
/**
 * Turns the per-architecture build outputs into the release asset set:
 *   - latest.json: the signed updater manifest `tauri-plugin-updater` reads,
 *     pointing at this release's installers (only when `.sig` files exist);
 *   - SHA256SUMS.txt: checksums of every asset, `sha256sum -c` compatible.
 *
 * Usage:
 *   node scripts/release/assemble.mjs --dir <assets> --version <x.y.z>
 *     --repo <owner/name> [--notes-file <release-notes.md>]
 *
 * Expects installers named by Tauri: `<Product>_<version>_<x64|arm64>-setup.exe`,
 * each with an optional `<installer>.sig` beside it.
 */
import { createHash } from "node:crypto";
import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fail } from "./lib.mjs";

const SCRIPT = "assemble";
const argv = process.argv.slice(2);
/** @param {string} name */
const arg = (name) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 ? argv[i + 1] : undefined;
};

const dir = arg("dir");
const version = arg("version")?.replace(/^v/, "");
const repo = arg("repo");
const notesFile = arg("notes-file");
if (!dir || !version || !repo) fail(SCRIPT, "usage: --dir <assets> --version <x.y.z> --repo <owner/name>");

/** Tauri's NSIS arch suffix → updater platform key. */
const PLATFORMS = { x64: "windows-x86_64", arm64: "windows-aarch64" };

const files = readdirSync(dir);
const platforms = {};
for (const [arch, platform] of Object.entries(PLATFORMS)) {
  const installer = files.find((f) => f.endsWith(`_${version}_${arch}-setup.exe`));
  if (!installer) fail(SCRIPT, `no ${arch} installer for ${version} in ${dir}`);
  if (!files.includes(`${installer}.sig`)) continue;
  const entry = {
    signature: readFileSync(join(dir, `${installer}.sig`), "utf8").trim(),
    url: `https://github.com/${repo}/releases/download/v${version}/${encodeURIComponent(installer)}`,
  };
  // NOTE: the updater looks up `<os>-<arch>-<installer>` before `<os>-<arch>`;
  // publishing both keeps NSIS selected even if an MSI is ever added.
  platforms[platform] = entry;
  platforms[`${platform}-nsis`] = entry;
}

const signedCount = Object.keys(platforms).length / 2;
if (signedCount === Object.keys(PLATFORMS).length) {
  const manifest = {
    version,
    notes: notesFile
      ? readFileSync(notesFile, "utf8").trim()
      : `See https://github.com/${repo}/releases/tag/v${version}`,
    pub_date: new Date().toISOString().replace(/\.\d{3}Z$/, "Z"),
    platforms,
  };
  writeFileSync(join(dir, "latest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  console.log(`${SCRIPT}: wrote latest.json`);
} else if (signedCount > 0) {
  fail(SCRIPT, "only some installers have updater signatures; refusing a partial latest.json");
} else {
  console.log(`${SCRIPT}: no updater signatures; skipping latest.json (updates disabled for this release)`);
}

const sums = readdirSync(dir)
  .filter((f) => f !== "SHA256SUMS.txt")
  .sort()
  .map((f) => `${createHash("sha256").update(readFileSync(join(dir, f))).digest("hex")}  ${f}`);
writeFileSync(join(dir, "SHA256SUMS.txt"), `${sums.join("\n")}\n`);
console.log(`${SCRIPT}: wrote SHA256SUMS.txt (${sums.length} files)`);
