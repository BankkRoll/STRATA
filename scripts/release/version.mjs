#!/usr/bin/env node
/**
 * Keeps the release version in sync across the Cargo workspace, Cargo.lock,
 * src-tauri/tauri.conf.json and the package.json files. Setting a version
 * also moves CHANGELOG.md's `[Unreleased]` notes under that version.
 *
 * Usage:
 *   node scripts/release/version.mjs              Print the current version.
 *   node scripts/release/version.mjs 0.2.0        Set every file to 0.2.0.
 *   node scripts/release/version.mjs --check      Fail unless every file agrees.
 *   node scripts/release/version.mjs --check 0.2.0
 *                                                 ...and agrees on 0.2.0 (used
 *                                                 by the release workflow with
 *                                                 the tag's version).
 */
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { promoteUnreleased } from "./changelog.mjs";
import { ROOT, fail } from "./lib.mjs";

const SCRIPT = "version";
// SemVer 2.0.0 without build metadata, which Windows installers can't carry.
const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$/;
const PACKAGE_JSONS = ["package.json", "ui/package.json"];

const read = (rel) => readFileSync(join(ROOT, rel), "utf8");
const write = (rel, text) => writeFileSync(join(ROOT, rel), text);

/** Matches `version = "..."` inside the `[workspace.package]` table; group 2 is the version. */
const WORKSPACE_VERSION = /(\[workspace\.package\][^[]*?\nversion\s*=\s*")([^"]*)(")/;

/**
 * Collects the version each file currently declares.
 *
 * @returns {Map<string, string | undefined>} file (or file:package) → version
 */
function currentVersions() {
  const found = new Map();
  found.set("Cargo.toml", read("Cargo.toml").match(WORKSPACE_VERSION)?.[2]);
  for (const [, name, version] of lockMembers(read("Cargo.lock"))) {
    found.set(`Cargo.lock:${name}`, version);
  }
  found.set("src-tauri/tauri.conf.json", JSON.parse(read("src-tauri/tauri.conf.json")).version);
  for (const file of PACKAGE_JSONS) found.set(file, JSON.parse(read(file)).version);
  return found;
}

/**
 * Yields workspace members from Cargo.lock: path packages carry no `source`.
 *
 * @param {string} lock - Cargo.lock contents.
 * @returns {Generator<[string, string, string]>} [whole entry, name, version]
 */
function* lockMembers(lock) {
  for (const m of lock.matchAll(/\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"\n(?!source = )/g)) {
    yield [m[0], m[1], m[2]];
  }
}

/**
 * Rewrites every version field to `next`.
 *
 * @param {string} next - New SemVer version.
 * @returns {void}
 */
function setVersion(next) {
  const cargo = read("Cargo.toml");
  if (!WORKSPACE_VERSION.test(cargo)) fail(SCRIPT, "no version in [workspace.package] of Cargo.toml");
  write("Cargo.toml", cargo.replace(WORKSPACE_VERSION, `$1${next}$3`));

  // NOTE: members inherit `version.workspace = true`, so only the lockfile
  // still names the old version; patching it avoids a network `cargo update`.
  const lock = read("Cargo.lock");
  write(
    "Cargo.lock",
    lock.replace(
      /(\[\[package\]\]\nname = "[^"]+"\nversion = ")([^"]+)("\n)(?!source = )/g,
      `$1${next}$3`,
    ),
  );

  // Text edits rather than JSON.stringify, so hand formatting survives.
  for (const file of ["src-tauri/tauri.conf.json", ...PACKAGE_JSONS]) {
    const text = read(file);
    let updated;
    if ("version" in JSON.parse(text)) {
      updated = text.replace(/("version"\s*:\s*")[^"]*(")/, `$1${next}$2`);
    } else {
      const name = text.match(/^([ \t]*)"name"\s*:\s*"[^"]*",\r?\n/m);
      if (!name) fail(SCRIPT, `${file} has no "name" line to put "version" after`);
      updated = text.replace(name[0], `${name[0]}${name[1]}"version": "${next}",\n`);
    }
    if (JSON.parse(updated).version !== next) fail(SCRIPT, `could not update the version in ${file}`);
    write(file, updated);
  }
}

const args = process.argv.slice(2);
const check = args.includes("--check");
const wanted = args.find((a) => !a.startsWith("--"))?.replace(/^v/, "");
if (wanted && !SEMVER.test(wanted)) fail(SCRIPT, `not a SemVer version: ${wanted}`);

if (wanted && !check) {
  promoteUnreleased(wanted);
  setVersion(wanted);
  console.log(`${SCRIPT}: set ${wanted}; review with \`git diff\` and commit`);
  process.exit(0);
}

const versions = currentVersions();
const expected = wanted ?? versions.get("Cargo.toml");
const wrong = [...versions].filter(([, v]) => v !== expected);
if (check) {
  if (wrong.length > 0) {
    fail(
      SCRIPT,
      `expected ${expected} everywhere:\n${wrong.map(([f, v]) => `  ${f}: ${v ?? "(missing)"}`).join("\n")}\n` +
        `Run \`node scripts/release/version.mjs ${expected}\` and commit.`,
    );
  }
  console.log(`${SCRIPT}: ${expected} in all ${versions.size} places`);
} else {
  console.log(expected);
}
