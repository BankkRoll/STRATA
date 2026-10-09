# Releasing Strata

Strata is released from GitHub Actions (`.github/workflows/release.yml`). A run
builds the x64 and ARM64 installers, the updater manifest, checksums and
third-party notices, and attaches them to a **draft** GitHub Release. Nothing
reaches users until you publish the draft.

## Release assets

| Asset | What it is |
|---|---|
| `Strata_<version>_x64-setup.exe` | Installer for x64 PCs |
| `Strata_<version>_arm64-setup.exe` | Installer for ARM64 PCs |
| `Strata_<version>_<arch>-setup.exe.sig` | Updater signatures for the installers |
| `latest.json` | Update manifest read by installed copies of Strata |
| `THIRD_PARTY_NOTICES.md` | Licenses of every bundled Rust crate and npm package (also shipped inside the app) |
| `SHA256SUMS.txt` | SHA-256 of every other asset (`sha256sum -c SHA256SUMS.txt`) |

The installer names are stable; download links and the update manifest depend on them.

## One-time setup

### Updater key (required for updates)

Installed copies only accept updates signed with the project's updater key.

1. Generate a key pair on a trusted machine, outside the repository:

   ```sh
   pnpm tauri signer generate -w <somewhere-safe>/strata-updater.key
   ```

2. Add repository **secrets** (Settings → Secrets and variables → Actions):
   - `TAURI_SIGNING_PRIVATE_KEY`: the contents of `strata-updater.key`
   - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: its password (omit if it has none)

3. Publish the **public** key, either way:
   - replace the `REPLACE_WITH_YOUR_UPDATER_PUBLIC_KEY ...` value of
     `plugins.updater.pubkey` in `src-tauri/tauri.conf.json` with the contents of
     `strata-updater.key.pub` and commit it (recommended: local builds match CI), or
   - set a repository **variable** `TAURI_UPDATER_PUBKEY` to it.

Back up the private key and password. If they are lost, existing installations
can no longer be updated and users must reinstall manually.

Without `TAURI_SIGNING_PRIVATE_KEY` the workflow still builds installers but
skips `latest.json` and logs a warning; such a release is never offered as an update.

### Optional: code signing

Releases currently ship **unsigned**, so Windows SmartScreen may ask users to
confirm before running the installer. The workflow logs
`Code signing skipped` and continues.

Signing turns on automatically, with no workflow edits, once all six of these
repository secrets exist for an
[Azure Artifact Signing](https://learn.microsoft.com/azure/artifact-signing/)
(formerly Trusted Signing) account:

| Secret | Value |
|---|---|
| `AZURE_TENANT_ID` | Directory (tenant) ID of the app registration |
| `AZURE_CLIENT_ID` | Application (client) ID of the app registration |
| `AZURE_CLIENT_SECRET` | A client secret of the app registration |
| `AZURE_SIGNING_ENDPOINT` | Regional endpoint, e.g. `https://eus.codesigning.azure.net` |
| `AZURE_SIGNING_ACCOUNT` | Artifact Signing account name |
| `AZURE_SIGNING_PROFILE` | Certificate profile name |

The app registration needs the *Artifact Signing Certificate Profile Signer*
role on the account. When signing is on, the app, the helper, the installer and
its uninstaller are all signed before the updater signature is computed, and a
signing failure fails the build. Setting only some of the secrets also fails
the build, so a misconfiguration can't silently produce unsigned releases.

## Cutting a release

1. Describe the release under `## [Unreleased]` in `CHANGELOG.md`, in user-facing
   language.
2. Set the version everywhere and move the changelog notes under it:

   ```sh
   node scripts/release/version.mjs 0.2.0
   git diff                    # Cargo.toml, Cargo.lock, tauri.conf.json, package.json files, CHANGELOG.md
   git commit -am "chore: release 0.2.0"
   ```

   Versions are SemVer. A pre-release (`0.2.0-beta.1`) becomes a GitHub
   pre-release and reaches only the beta update channel.
3. Tag and push:

   ```sh
   git tag v0.2.0
   git push origin main v0.2.0
   ```

   The workflow fails early if the tag, the files and the changelog disagree.
   You can also start it from the Actions tab (**Run workflow**); it then uses
   the version in the files and tags the commit when the draft is published.
4. When the run finishes, open the draft release, check the notes and assets,
   and **Publish**. Publishing makes it the download on `releases/latest`, the
   stable update, and (through the `Update beta channel` job) the beta update.

To redo a release, delete the draft (and the tag, if it was pushed) and run
again; the workflow refuses to overwrite an existing release.

## How updates reach users

- Installed copies check about 20 seconds after launch and then daily, then
  download and verify the update in the background and install it when Strata
  exits. The per-machine installer asks for administrator approval at that point.
- Stable follows `releases/latest/download/latest.json`; beta follows the
  `updater-beta` pre-release that the workflow repoints on every publish.
- If a new version is installed but its window never finishes loading, the next
  launch offers to reinstall the previous version, using that release's own
  `latest.json` and the same signature check. Crashes before Strata's startup
  code runs can't be detected; users then reinstall from the Releases page.

## Building locally

```sh
pnpm tauri build --bundles nsis
```

produces an unsigned installer without the helper or updater artifacts, the
same as CI's smoke builds. To reproduce a release build:

```sh
cargo install cargo-about --locked --features cli
node scripts/release/licenses.mjs
pnpm tauri build --bundles nsis --config src-tauri/tauri.release.conf.json
```

This needs `TAURI_SIGNING_PRIVATE_KEY` (any test key from
`pnpm tauri signer generate`) and builds `strata-helper`; set
`STRATA_HELPER_EXE` to use a prebuilt helper instead.

## Scripts

| Script | Purpose |
|---|---|
| `scripts/release/version.mjs` | Print, set (`<version>`) or verify (`--check [version]`) the version everywhere |
| `scripts/release/changelog.mjs` | Print a version's release notes from `CHANGELOG.md` |
| `scripts/release/licenses.mjs` | Generate `THIRD_PARTY_NOTICES.md` (policy in `scripts/release/about.toml`) |
| `scripts/release/stage-helper.mjs` | Build and stage the `strata-helper` sidecar |
| `scripts/release/sign.mjs` | Optional Authenticode signing hook used by the Tauri bundler |
| `scripts/release/assemble.mjs` | Write `latest.json` and `SHA256SUMS.txt` from the build outputs |
