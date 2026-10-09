#!/usr/bin/env node
/**
 * Generates THIRD_PARTY_NOTICES.md: every Rust crate and npm package that
 * ships inside the installer, grouped by license text. The release config
 * bundles it as an app resource for the About screen, and the release
 * workflow attaches it to the GitHub Release.
 *
 * Rust: `cargo about` (JSON output; policy in scripts/release/about.toml) for
 * strata-app and, once it exists, strata-helper.
 * npm: `pnpm licenses list --prod` for the UI package, with each package's
 * LICENSE file read from node_modules.
 *
 * Usage: node scripts/release/licenses.mjs [--out <file>]
 *   Default output: target/release-stage/THIRD_PARTY_NOTICES.md
 */
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { ROOT, STAGE_DIR, capture, fail } from "./lib.mjs";

const SCRIPT = "licenses";
const argv = process.argv.slice(2);
const outIndex = argv.indexOf("--out");
const out = outIndex >= 0 ? argv[outIndex + 1] : join(STAGE_DIR, "THIRD_PARTY_NOTICES.md");

const manifests = ["src-tauri/Cargo.toml", "crates/strata-helper/Cargo.toml"].filter((m) =>
  existsSync(join(ROOT, m)),
);

/** @type {Map<string, {name: string, users: Set<string>}>} license text → crates using it */
const rustTexts = new Map();
for (const manifest of manifests) {
  // NOTE: cargo-about refuses to write to a pipe when launched from
  // PowerShell (encoding concerns), so it always writes to a file.
  const jsonFile = join(STAGE_DIR, "cargo-about.json");
  mkdirSync(STAGE_DIR, { recursive: true });
  let json;
  try {
    capture("cargo", [
      "about", "generate", "--format", "json", "--locked", "--fail",
      "-m", manifest, "-c", "scripts/release/about.toml", "-o", jsonFile,
    ]);
    json = readFileSync(jsonFile, "utf8");
  } catch (e) {
    fail(
      SCRIPT,
      `cargo about failed for ${manifest}. Install it with ` +
        "`cargo install cargo-about --locked --features cli` and accept any new " +
        `license in scripts/release/about.toml after review.\n${e.message}`,
    );
  }
  for (const license of JSON.parse(json).licenses) {
    const text = license.text.trim();
    const entry = rustTexts.get(text) ?? { name: license.name, users: new Set() };
    for (const { crate } of license.used_by) entry.users.add(`${crate.name} ${crate.version}`);
    rustTexts.set(text, entry);
  }
}

const LICENSE_FILE = /^(licen[sc]e|copying|notice)(\.|-|$)/i;

/**
 * Reads the license files shipped in an npm package directory.
 *
 * @param {string} dir - Installed package directory.
 * @returns {string}
 */
function npmLicenseText(dir) {
  if (!existsSync(dir)) return "";
  return readdirSync(dir)
    .filter((f) => LICENSE_FILE.test(f))
    .sort()
    .map((f) => readFileSync(join(dir, f), "utf8").trim())
    .join("\n\n");
}

// COMPAT: on Windows pnpm is a .cmd shim, which Node can only start through a
// shell. The arguments are fixed literals, so the shell sees nothing untrusted.
const npmReport = JSON.parse(
  capture("pnpm", ["--filter", "@strata/ui", "licenses", "list", "--prod", "--json"], {
    shell: process.platform === "win32",
  }),
);
/** @type {{id: string, license: string, homepage?: string, text: string}[]} */
const npmPackages = [];
for (const [license, packages] of Object.entries(npmReport)) {
  for (const pkg of packages) {
    // NOTE: `@types/*` packages are type-only and never reach the bundle.
    if (pkg.name.startsWith("@types/")) continue;
    for (const [i, version] of pkg.versions.entries()) {
      npmPackages.push({
        id: `${pkg.name} ${version}`,
        license,
        homepage: pkg.homepage,
        text: npmLicenseText(pkg.paths?.[i] ?? ""),
      });
    }
  }
}
npmPackages.sort((a, b) => a.id.localeCompare(b.id));

const fence = (text) => {
  const ticks = "`".repeat(Math.max(3, ...[...text.matchAll(/`+/g)].map((m) => m[0].length + 1)));
  return `${ticks}text\n${text}\n${ticks}`;
};

// SECURITY: this file ships to users; it must contain package metadata and
// license texts only, never local paths from the build machine.
const lines = [
  "# Third-party notices",
  "",
  "Strata is MIT licensed. It includes the following third-party software.",
  "",
  "## Rust crates",
  "",
];
const rustSorted = [...rustTexts.entries()].sort(
  (a, b) => a[1].name.localeCompare(b[1].name) || [...a[1].users][0].localeCompare([...b[1].users][0]),
);
for (const [text, { name, users }] of rustSorted) {
  lines.push(`### ${name}`, "", `Used by: ${[...users].sort().join(", ")}`, "", fence(text), "");
}
lines.push("## npm packages", "");
for (const pkg of npmPackages) {
  lines.push(`### ${pkg.id} (${pkg.license})`, "");
  if (pkg.homepage) lines.push(pkg.homepage, "");
  lines.push(pkg.text ? fence(pkg.text) : `License: ${pkg.license} (no license file in the package)`, "");
}

mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, `${lines.join("\n").trimEnd()}\n`);
const crateCount = new Set([...rustTexts.values()].flatMap((e) => [...e.users])).size;
console.log(`${SCRIPT}: ${crateCount} crates, ${npmPackages.length} npm packages -> ${out}`);
