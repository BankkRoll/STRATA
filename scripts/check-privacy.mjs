#!/usr/bin/env node
/**
 * Fails when tracked files contain machine-specific identifiers: real user
 * profile paths, real account SIDs, or paths into local agent worktrees.
 * Placeholders used in docs and tests are allow-listed below.
 *
 * Usage: node scripts/check-privacy.mjs
 */
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";

/** Profile names that are generic placeholders or Windows built-ins. */
const ALLOWED_PROFILES = new Set([
  "me", "you", "user", "alice", "bob", "carol", "dave", "someone", "other", "guest",
  "test", "name", "x", "a", "b", ".", "..", "*", "public", "default", "default user",
  "all users", "nobody-strata-test", "<user>", "<name>", "{name}", "%username%",
  "username", "example", "docs",
]);

const SKIP = [
  /^pnpm-lock\.yaml$/,
  /^Cargo\.lock$/,
  /\.(png|ico|icns|bin)$/,
  // NOTE: this file documents the patterns it rejects.
  /^scripts\/check-privacy\.mjs$/,
];

/** Matches `X:\Users\<name>` with `\`, `\\` or `/` separators; group 1 is the name. */
const PROFILE_PATH = /[A-Za-z]:[\\/]+Users[\\/]+([^\\/"'`\s,)]+)/g;

/** Real domain/machine SIDs have three large sub-authorities; placeholders don't. */
const REAL_SID = /S-1-5-21-\d{6,}-\d{6,}-\d{6,}/;

const WORKTREE_PATH = /\.claude[\\/]worktrees/;

/** A GUID on a line that talks about volumes; group 1 is the GUID body. */
const VOLUME_GUID = /\{([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\}/gi;
const VOLUME_CONTEXT = /volume|guid/i;

/**
 * Whether a GUID looks randomly generated rather than hand-written.
 * Placeholders like `11111111-1111-4111-8111-111111111111` use few distinct
 * digits; real volume GUIDs use most of the hex alphabet.
 *
 * @param {string} guid - GUID without braces.
 * @returns {boolean}
 */
function looksReal(guid) {
  const hex = guid.toLowerCase().replaceAll("-", "");
  if (hex.includes("12345678") || hex.includes("abcdef")) return false;
  return new Set(hex).size >= 10;
}

/**
 * Returns the privacy problems found on one line.
 *
 * @param {string} line - A single line of a tracked file.
 * @returns {string[]} Human-readable problem descriptions.
 */
function problemsIn(line) {
  const found = [];
  for (const m of line.matchAll(PROFILE_PATH)) {
    const name = m[1].toLowerCase();
    // NOTE: `$...`, `%...%`, `{...}` and `<...>` are metadata names, env vars,
    // tokens or placeholders, never a real account.
    if (/^[$%{<]/.test(name) || ALLOWED_PROFILES.has(name)) continue;
    found.push(`user profile path (${m[0]})`);
  }
  if (REAL_SID.test(line)) found.push("real account SID");
  if (WORKTREE_PATH.test(line)) found.push("local worktree path");
  if (VOLUME_CONTEXT.test(line)) {
    for (const m of line.matchAll(VOLUME_GUID)) {
      if (looksReal(m[1])) found.push(`real-looking volume GUID (${m[0]})`);
    }
  }
  return found;
}

const files = execFileSync("git", ["ls-files", "-z"], { encoding: "utf8" })
  .split("\0")
  .filter((f) => f && !SKIP.some((re) => re.test(f)));

let count = 0;
for (const file of files) {
  let text;
  try {
    text = readFileSync(file, "utf8");
  } catch {
    continue;
  }
  text.split(/\r?\n/).forEach((line, i) => {
    for (const why of problemsIn(line)) {
      console.error(`${file}:${i + 1}: ${why}`);
      count++;
    }
  });
}

if (count > 0) {
  console.error(`\n${count} privacy problem(s). Use placeholders such as C:\\Users\\me.`);
  process.exit(1);
}
console.log(`privacy check: ${files.length} files clean`);
