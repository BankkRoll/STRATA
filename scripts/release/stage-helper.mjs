#!/usr/bin/env node
/**
 * Builds the elevated `strata-helper` binary and stages it where
 * `src-tauri/tauri.release.conf.json` expects the sidecar
 * (`target/release-stage/strata-helper-<triple>.exe`), so the installer ships
 * it next to the app.
 *
 * Runs as part of the release `beforeBuildCommand`. Fails loudly when the
 * helper is missing: an installer without it would silently lose fast scans
 * and service mode (SPEC §4).
 *
 * Usage:
 *   node scripts/release/stage-helper.mjs [--target <triple>]
 *
 * Environment:
 *   STRATA_HELPER_EXE  Use this prebuilt helper instead of building one.
 *   CARGO_TARGET_DIR   Honoured when locating cargo's output.
 */
import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { ROOT, STAGE_DIR, fail, run, targetTriple } from "./lib.mjs";

const SCRIPT = "stage-helper";
const triple = targetTriple(process.argv.slice(2));
const dest = join(STAGE_DIR, `strata-helper-${triple}.exe`);

let source = process.env.STRATA_HELPER_EXE;
if (source) {
  if (!existsSync(source)) fail(SCRIPT, `STRATA_HELPER_EXE points at a missing file: ${source}`);
} else {
  if (!existsSync(join(ROOT, "crates", "strata-helper", "Cargo.toml"))) {
    fail(
      SCRIPT,
      "crates/strata-helper does not exist yet. Release installers must bundle the " +
        "elevated helper. Land the strata-helper crate, or set STRATA_HELPER_EXE to a " +
        "prebuilt binary for local testing.",
    );
  }
  run("cargo", ["build", "--release", "--locked", "-p", "strata-helper", "--target", triple]);
  const targetDir = process.env.CARGO_TARGET_DIR || join(ROOT, "target");
  source = join(targetDir, triple, "release", "strata-helper.exe");
  if (!existsSync(source)) fail(SCRIPT, `cargo finished but ${source} is missing`);
}

mkdirSync(STAGE_DIR, { recursive: true });
copyFileSync(source, dest);
console.log(`${SCRIPT}: staged ${triple} helper`);
