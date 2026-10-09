/**
 * Shared helpers for the release scripts: repo paths, process spawning and
 * target-triple resolution. Node built-ins only.
 */
import { execFileSync, spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** Absolute path of the repository root. */
export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");

/**
 * Staging area for generated release inputs (helper sidecar, notices). Lives
 * under the git-ignored repo-root `target/`, independent of `CARGO_TARGET_DIR`,
 * because `tauri.release.conf.json` references it by a fixed relative path.
 */
export const STAGE_DIR = join(ROOT, "target", "release-stage");

/**
 * Prints `message` prefixed with the script name and exits with status 1.
 *
 * @param {string} script - Short script name for the log prefix.
 * @param {string} message - What went wrong and how to fix it.
 * @returns {never}
 */
export function fail(script, message) {
  console.error(`${script}: error: ${message}`);
  process.exit(1);
}

/**
 * Runs a command with inherited stdio and throws on a non-zero exit.
 *
 * @param {string} cmd - Executable name or path.
 * @param {string[]} args - Arguments, passed without a shell.
 * @param {import("node:child_process").SpawnSyncOptions} [options]
 * @returns {void}
 */
export function run(cmd, args, options = {}) {
  const result = spawnSync(cmd, args, { stdio: "inherit", cwd: ROOT, ...options });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${cmd} ${args.join(" ")} exited with status ${result.status}`);
  }
}

/**
 * Runs a command and returns its stdout as a string.
 *
 * @param {string} cmd - Executable name or path.
 * @param {string[]} args - Arguments, passed without a shell.
 * @returns {string}
 */
export function capture(cmd, args) {
  return execFileSync(cmd, args, { cwd: ROOT, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 });
}

/**
 * Resolves the Rust target triple being built: `--target <triple>` on the
 * command line, then `TAURI_ENV_TARGET_TRIPLE` (set by the Tauri CLI for
 * `beforeBuildCommand`), then the host triple from `rustc -vV`.
 *
 * @param {string[]} argv - Script arguments.
 * @returns {string}
 */
export function targetTriple(argv) {
  const i = argv.indexOf("--target");
  if (i >= 0 && argv[i + 1]) return argv[i + 1];
  if (process.env.TAURI_ENV_TARGET_TRIPLE) return process.env.TAURI_ENV_TARGET_TRIPLE;
  const host = capture("rustc", ["-vV"]).match(/^host: (\S+)$/m);
  if (!host) throw new Error("could not read the host triple from `rustc -vV`");
  return host[1];
}
