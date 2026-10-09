// Bundle budget check, run after `vite build`.
//
// Keeps cold start fast (SPEC §2: first paint ≤ 600 ms) by capping the entry
// chunk's gzip size, requires the non-default views to stay in lazy chunks,
// and fails if the dev-only fixture harness leaks into production output.
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";

const ENTRY_GZIP_BUDGET = 130 * 1024;
const LAZY = ["ArcRenderer", "CircleRenderer", "CommandPalette"];
const FORBIDDEN = ["__fixtures__", "Fixture harness", "fixtureServices"];

const dir = join(import.meta.dirname, "..", "dist", "assets");
const files = readdirSync(dir).filter((f) => f.endsWith(".js"));
const failures = [];
const entry = files.find((f) => f.startsWith("index-"));
if (!entry) failures.push("no entry chunk found");
for (const f of files) {
  const src = readFileSync(join(dir, f));
  const gz = gzipSync(src).length;
  console.log(`${f.padEnd(36)} ${(src.length / 1024).toFixed(1).padStart(7)} kB  gzip ${(gz / 1024).toFixed(1).padStart(6)} kB`);
  if (f === entry && gz > ENTRY_GZIP_BUDGET) failures.push(`${f}: ${gz} B gzip exceeds ${ENTRY_GZIP_BUDGET} B`);
  const text = src.toString("utf8");
  for (const s of FORBIDDEN) if (text.includes(s)) failures.push(`${f} contains dev-only "${s}"`);
}
for (const name of LAZY) {
  if (!files.some((f) => f.startsWith(`${name}-`))) failures.push(`${name} is not a lazy chunk`);
}
if (failures.length > 0) {
  console.error(failures.join("\n"));
  process.exit(1);
}
console.log("bundle budget ok");
