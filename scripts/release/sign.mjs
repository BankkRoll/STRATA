#!/usr/bin/env node
/**
 * Authenticode-signs one file with Azure Artifact Signing (formerly Trusted
 * Signing). Tauri calls this as `bundle > windows > signCommand` (see
 * `src-tauri/tauri.release.conf.json`) for the app exe, the helper sidecar,
 * the NSIS plugins, the uninstaller and the installer, before it computes the
 * updater signature, so the signed bytes are what the updater verifies.
 *
 * Signing is optional. Without AZURE_SIGNING_DLIB and AZURE_SIGNING_METADATA
 * the script exits 0 without touching the file; the release workflow logs the
 * single "signing skipped" line. Set STRATA_REQUIRE_SIGNING=1 to fail instead.
 *
 * Environment:
 *   AZURE_SIGNING_DLIB      Path to Azure.CodeSigning.Dlib.dll (x64).
 *   AZURE_SIGNING_METADATA  Path to the dlib's metadata.json.
 *   AZURE_TENANT_ID, AZURE_CLIENT_ID, AZURE_CLIENT_SECRET
 *                           Read by the dlib's DefaultAzureCredential.
 *   SIGNTOOL                Optional path to signtool.exe.
 *   SIGN_TIMESTAMP_URL      Optional RFC 3161 timestamp server.
 *
 * Usage: node scripts/release/sign.mjs <file>
 */
import { existsSync, readdirSync } from "node:fs";
import { basename, join } from "node:path";
import { spawnSync } from "node:child_process";

const SCRIPT = "sign";
const env = process.env;
const file = process.argv[2];
if (!file || !existsSync(file)) {
  console.error(`${SCRIPT}: error: file to sign not found: ${file ?? "(none)"}`);
  process.exit(1);
}

if (!env.AZURE_SIGNING_DLIB || !env.AZURE_SIGNING_METADATA) {
  if (env.STRATA_REQUIRE_SIGNING === "1") {
    console.error(`${SCRIPT}: error: STRATA_REQUIRE_SIGNING=1 but no signing credentials are set`);
    process.exit(1);
  }
  process.exit(0);
}

/**
 * Finds the newest x64 signtool.exe in the Windows 10/11 SDK.
 *
 * @returns {string}
 */
function findSigntool() {
  if (env.SIGNTOOL) return env.SIGNTOOL;
  const kits = join(env["ProgramFiles(x86)"] ?? "C:\\Program Files (x86)", "Windows Kits", "10", "bin");
  const versions = existsSync(kits)
    ? readdirSync(kits)
        .filter((v) => /^\d+\.\d+\.\d+\.\d+$/.test(v))
        .sort((a, b) => b.localeCompare(a, undefined, { numeric: true }))
    : [];
  for (const v of versions) {
    // NOTE: the Azure dlib is x64-only, so always use the x64 signtool, even
    // when the binary being signed is ARM64.
    const candidate = join(kits, v, "x64", "signtool.exe");
    if (existsSync(candidate)) return candidate;
  }
  console.error(`${SCRIPT}: error: signtool.exe not found; install the Windows SDK or set SIGNTOOL`);
  process.exit(1);
}

const signtool = findSigntool();
const args = [
  "sign", "/fd", "SHA256", "/td", "SHA256",
  "/tr", env.SIGN_TIMESTAMP_URL || "http://timestamp.acs.microsoft.com",
  "/dlib", env.AZURE_SIGNING_DLIB,
  "/dmdf", env.AZURE_SIGNING_METADATA,
  file,
];

// NOTE: the timestamp server and the signing endpoint fail transiently often
// enough to break a release build, so retry with a short backoff.
for (let attempt = 1; ; attempt++) {
  const result = spawnSync(signtool, args, { stdio: "inherit" });
  if (result.status === 0) break;
  if (attempt === 3) {
    console.error(`${SCRIPT}: error: signtool failed for ${basename(file)}`);
    process.exit(1);
  }
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, attempt * 5000);
}

const verify = spawnSync(signtool, ["verify", "/pa", "/q", file], { stdio: "inherit" });
if (verify.status !== 0) {
  console.error(`${SCRIPT}: error: signature on ${basename(file)} does not verify`);
  process.exit(1);
}
console.log(`${SCRIPT}: signed ${basename(file)}`);
