#!/usr/bin/env node
/**
 * Reads and updates CHANGELOG.md (Keep a Changelog format).
 *
 * Usage:
 *   node scripts/release/changelog.mjs <version>
 *     Prints the body of the `## [<version>]` section, the release notes.
 *     Fails if the section is missing or empty.
 *
 * `version.mjs` uses {@link promoteUnreleased} to turn `## [Unreleased]` into
 * the new version's section.
 */
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { ROOT, fail } from "./lib.mjs";

const SCRIPT = "changelog";
const FILE = join(ROOT, "CHANGELOG.md");

const escape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/**
 * Returns the trimmed body of the `## [<name>]` section, or `undefined`.
 *
 * @param {string} text - CHANGELOG.md contents.
 * @param {string} name - Section name, e.g. `0.2.0` or `Unreleased`.
 * @returns {string | undefined}
 */
export function section(text, name) {
  const m = text.match(new RegExp(`^## \\[${escape(name)}\\][^\\n]*\\n([\\s\\S]*?)(?=^## \\[|(?![\\s\\S]))`, "m"));
  return m?.[1].trim();
}

/**
 * Renames `## [Unreleased]` to `## [<version>] - <date>` and opens a fresh,
 * empty `## [Unreleased]` above it. A no-op if the version already has a section.
 *
 * @param {string} version - New SemVer version.
 * @param {string} [date] - ISO date; defaults to today (UTC).
 * @returns {void}
 */
export function promoteUnreleased(version, date = new Date().toISOString().slice(0, 10)) {
  const text = readFileSync(FILE, "utf8");
  if (section(text, version) !== undefined) return;
  if (!section(text, "Unreleased")) {
    fail(SCRIPT, "CHANGELOG.md has nothing under ## [Unreleased]; describe the release first");
  }
  writeFileSync(FILE, text.replace(/^## \[Unreleased\][^\n]*$/m, `## [Unreleased]\n\n## [${version}] - ${date}`));
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const version = process.argv[2]?.replace(/^v/, "");
  if (!version) fail(SCRIPT, "usage: changelog.mjs <version>");
  const body = section(readFileSync(FILE, "utf8"), version);
  if (!body) {
    fail(
      SCRIPT,
      `CHANGELOG.md has no notes for ${version}. Run \`node scripts/release/version.mjs ${version}\` ` +
        "after describing the release under ## [Unreleased].",
    );
  }
  process.stdout.write(`${body}\n`);
}
