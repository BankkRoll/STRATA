# Release track (M14): installer, updater, signing, release workflow

How to cut a release: [docs/RELEASING.md](../RELEASING.md).

## Decisions

1. **Open source, no licensing.** Strata is MIT open source, provided as-is and
   not actively maintained. The paid-license parts of SPEC §20 are dropped: no
   license keys, activation, feature gating, checkout URL or `strata://activate`
   protocol handler. Kept: per-machine installer, signed updates, optional code
   signing, no telemetry. (`strata_store::LicenseRecord` is now unused; removing it
   is up to the store owner.)
2. **Releases ship unsigned for now** (owner decision). Authenticode signing via
   Azure Artifact Signing turns on automatically when its six secrets exist; with
   none, the workflow logs one `Code signing skipped` notice and succeeds. Partial
   configuration fails the build. Only Azure is supported (no PFX path) to keep
   one well-tested route.
3. **NSIS only, per machine.** `bundle.targets = ["nsis"]`,
   `nsis.installMode = "perMachine"` (service mode needs Program Files and HKLM).
   MSI was dropped: one installer format means one updater artifact per arch.
4. **WebView2: `downloadBootstrapper`.** Both bootstrapper modes need internet;
   `embedBootstrapper` only adds ~1.8 MB to skip one download. Only
   `offlineInstaller` (+~127 MB) works offline, which breaks the 15 MB budget.
   Windows 11 and current Windows 10 22H2 already ship the runtime, so the
   bootstrapper almost never runs. Updates never reinstall WebView2.
5. **Release-only config overlay.** `src-tauri/tauri.release.conf.json` adds the
   helper sidecar, the notices resource, the sign command and updater artifacts.
   Plain `pnpm tauri build`, `cargo clippy` and CI's bundle job stay
   helper-free and key-free, so other tracks never need the helper or secrets.
6. **Updater hardening.** `requireSignedVersion: true`: the CLI (2.12) writes
   `version:` into the signature's trusted comment, so a tampered manifest can't
   pair a high version number with an old, validly signed installer.
7. **Beta channel** = rolling pre-release `updater-beta` holding a copy of the
   newest published release's `latest.json`; stable = `releases/latest`.
8. **No build cache in release builds** (cache poisoning can't reach users).

## Secrets and variables

| Name | Kind | Required | Purpose |
|---|---|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | secret | for updates | minisign key that signs installers for the updater |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | secret | if key has one | its password |
| `TAURI_UPDATER_PUBKEY` | variable | unless committed | public key, injected into the build if `tauri.conf.json` still has the placeholder |
| `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` | secrets | optional | Entra app used by the signing dlib |
| `AZURE_SIGNING_ENDPOINT`, `AZURE_SIGNING_ACCOUNT`, `AZURE_SIGNING_PROFILE` | secrets | optional | Artifact Signing account details |

The committed `plugins.updater.pubkey` is the placeholder
`REPLACE_WITH_YOUR_UPDATER_PUBLIC_KEY (see docs/RELEASING.md)`. While it is in
place (and no variable overrides it) the app disables update checks entirely.
If the private key secret is set but no public key is available, the release
build fails rather than shipping an app that can't verify its own updates.

## What was built

- `src-tauri/tauri.conf.json`: version, publisher/homepage/MIT metadata, NSIS
  per-machine + hooks, WebView2 mode, updater plugin config.
- `src-tauri/tauri.release.conf.json`: release overlay (above).
- `src-tauri/windows/hooks.nsh`: NSIS hooks.
  - Pre-install (also on updates): `sc stop StrataHelper`, then the template's
    Restart Manager prompt for a running `strata-helper.exe`, so the binary can
    be replaced.
  - Pre-uninstall (not in update mode): `strata-helper.exe --uninstall-service`,
    fallback `sc stop` + `sc delete StrataHelper`, `logman stop Strata-FileActivity -ets`,
    remove `StartupApproved\Run` and HKLM `Run` values. All best effort.
  - Already in Tauri's template and relied on: Start-menu shortcut, HKCU `Run`
    value removal, and the uninstall-page **"Delete the application data"**
    checkbox, which removes `%APPDATA%\app.strata.desktop` and
    `%LOCALAPPDATA%\app.strata.desktop` only when ticked.
- `src-tauri/src/updater.rs` + two `.plugin(...)` lines in `lib.rs`: an inline
  `strata-updater` plugin (background check, download, install on exit,
  rollback). Commands are not exposed yet; see hand-offs.
- `src-tauri/Cargo.toml`: `tauri-plugin-updater = "2.13"` and the workspace
  `windows` crate with `Win32_UI_WindowsAndMessaging` (already compiled in the
  tree by `strata-win`) for the native rollback prompt. A native message box is
  used deliberately: the rollback prompt must work when the frontend is broken.
- `scripts/release/`: `version.mjs`, `changelog.mjs`, `licenses.mjs` +
  `about.toml`, `stage-helper.mjs`, `sign.mjs`, `assemble.mjs`, `lib.mjs`. Node
  built-ins only.
- `.github/workflows/release.yml`, `CHANGELOG.md`, `docs/RELEASING.md`.

### Rollback design and its limits

1. Before installing an update (on app exit, or `updater::install_now`), the app
   writes `%LOCALAPPDATA%\app.strata.desktop\update-state.json` =
   `{pending: {from, to, launches: 0}}`.
2. Each launch of version `to` increments `launches` in plugin setup.
3. When the main webview fires `PageLoadEvent::Finished`, the record is cleared.
4. A launch of `to` that finds `launches >= 1` means the previous launch never
   finished loading: the record is cleared and a native Yes/No box offers to
   reinstall `from`. Yes fetches `releases/download/v<from>/latest.json` with a
   comparator that accepts exactly that version, verifies the signature and runs
   the installer (passive, `/UPDATE`). Failure shows the Releases URL.
5. Running any version other than `to` (UAC declined, or rollback done) clears
   the record.

Limits: a crash before plugin setup (very early Rust startup, missing DLL)
can't be detected; "page finished loading" is a proxy for first paint (a page
that loads and then throws counts as healthy); the user is asked once, never
nagged; a user who quits within the first second of a new version is asked on
the next launch (harmless). Rollback needs network access and GitHub.

## Verified locally (Windows 11 x64, unelevated)

- `cargo clippy -p strata-app --all-targets -- -D warnings`: clean.
  `cargo test -p strata-app`: 10 passed (7 new: launch decision table, endpoint
  derivation, policy serde names).
- `pnpm tauri build --bundles nsis` (base config): **2.91 MiB** x64 installer
  (3,051,375 bytes). Target ≤ 15 MB.
- Release overlay build with a local throwaway updater key (kept outside the
  repo), a stand-in helper exe (`STRATA_HELPER_EXE`) and no signing secrets:
  **2.98 MiB**, `.sig` produced with `version:0.1.0` in its trusted comment.
  The real helper will add roughly its own compressed size.
- Bundler log confirms `signCommand` (cwd = `src-tauri`, relative script path
  works) is invoked for the helper sidecar, `strata-app.exe`, the NSIS plugin
  DLLs, the uninstaller and the installer.
- Generated `installer.nsi`: includes `hooks.nsh`, `INSTALLMODE "perMachine"`,
  installs `strata-helper.exe` and `THIRD_PARTY_NOTICES.md`, and inserts all four
  hook points; `makensis` compiled the hooks. The installer was **not** run.
- `licenses.mjs`: 253 crates + 8 npm packages, ~280 KB, no local paths.
- `version.mjs` and `changelog.mjs` exercised on a scratch copy (set, idempotent
  re-run, `--check` with and without `v` prefix, bad SemVer, empty Unreleased).
- `assemble.mjs`: `latest.json` with `windows-x86_64[-nsis]` and
  `windows-aarch64[-nsis]`, `SHA256SUMS.txt`; skips the manifest when no `.sig`
  exists and refuses a partial one.
- `actionlint` 1.7.12: `release.yml` and `ci.yml` clean.

Not verified (needs the owner or a real release): installing/uninstalling,
the end-to-end update and rollback, Azure signing, the ARM64 build of the
release overlay (CI already builds ARM64 with the base config).

## Contracts and hand-offs

- **strata-helper track:** crate must be `crates/strata-helper` producing
  `strata-helper.exe`; register the service as **`StrataHelper`**; support
  `--uninstall-service` (stop + delete, exit 0 if not installed). Until the
  crate lands, release builds fail with a clear `stage-helper` error (by design).
- **ETW track:** session name **`Strata-FileActivity`**.
- **Settings track:** after loading settings and on change, call
  `strata_app_lib::updater::set_policy(&app, Policy { channel, auto_download })`
  (`Channel::{Stable, Beta}` matches `strata_store::UpdateChannel`).
  Autostart must use the value name `Strata` (product name) under
  `HKCU\...\Run`, which tauri-plugin-autostart does by default.
- **UI track:** listen for `strata://update-ready` / `strata://update-available`
  (`{version, notes}`); a "Restart now" button needs a command that calls
  `updater::install_now`. The About screen reads `THIRD_PARTY_NOTICES.md` from
  the resource dir (`BaseDirectory::Resource`); it exists only in release builds.
- **Lead:** root `Cargo.toml` still says `license = "LicenseRef-Proprietary"`
  (should be `MIT`; out of this track's scope). The root README's download names
  match the assets. README wording should note SmartScreen may ask for
  confirmation because builds are unsigned.
