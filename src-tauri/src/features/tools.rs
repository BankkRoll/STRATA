//! Built-in Windows tools and Recycle Bin facts (`ui/src/lib/tools.ts`).
//!
//! Every tool goes through a prepare/run round trip: `tools_prepare` builds
//! the exact command (`strata_clean::tools::CommandSpec`) **on the backend**
//! and returns it with a single-use prompt id; the UI shows the command line
//! and description; `tools_run` executes exactly the stored spec. The UI can
//! never supply a program or arguments: uninstallers are named by catalog
//! app id and their command comes from the catalog. Emptying the Recycle
//! Bin is the same round trip, with the prompt built from the bin's current
//! item count and size and redeemed as a `Consent`.
//!
//! Destructive tools (DISM component cleanup, emptying the bin) are written
//! ahead to the undo/audit log as `tool` actions. DISM runs through a UAC
//! prompt with its output captured.

use std::time::Instant;

use serde::{Deserialize, Serialize};
use strata_clean::consent::{EmptyRecycleBin, Prompt};
use strata_clean::tools::{
    self, CommandSpec, Launch, ToolAction, ToolContext, ToolError, ToolOutput,
};
use strata_clean::volume::{RecycleBinSupport, RecycleUnavailable};
use strata_core::{FileRef, FileTime, Safety};
use strata_store::{
    ActionKind, ActionStatus, DeleteMethod, ItemOutcome, ItemResult, PlannedItem, Store, VolumeKey,
};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use super::consent::PendingConsents;
use super::error::{ErrorKind, FeatureError, FeatureResult, blocking};

/// A tool the UI may ask for (`ToolAction` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolRequest {
    /// Empty the Recycle Bin of one drive (`C`, `C:` or `C:\`) or of every
    /// drive.
    EmptyRecycleBin {
        /// Drive; `None` for every drive.
        drive: Option<String>,
    },
    /// Windows Disk Cleanup, optionally for one drive letter.
    DiskCleanup {
        /// Drive letter.
        drive: Option<String>,
    },
    /// Storage Sense settings page.
    StorageSenseSettings,
    /// DISM component store cleanup (elevated, output captured).
    DismComponentCleanup,
    /// System Protection dialog.
    SystemProtection,
    /// Hibernation file guidance (never executed).
    HibernationGuidance,
    /// An app's own uninstaller, from the catalog.
    Uninstall {
        /// Catalog app id (`apps_footprint`).
        #[serde(rename = "appId")]
        app_id: String,
    },
    /// `compact /compactos:query` (read-only).
    CompactOsStatus,
}

fn drive_letter(d: Option<&str>) -> FeatureResult<Option<char>> {
    match d.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => {
            let c = s.chars().next().unwrap_or(' ');
            let rest = &s[c.len_utf8()..];
            if c.is_ascii_alphabetic() && matches!(rest, "" | ":" | ":\\") {
                Ok(Some(c.to_ascii_uppercase()))
            } else {
                Err(FeatureError::invalid(format!("{s} is not a drive")))
            }
        }
    }
}

/// A prepared tool waiting for confirmation.
#[derive(Debug)]
pub enum PreparedTool {
    /// A command to run exactly as shown.
    Command(CommandSpec),
    /// Emptying the Recycle Bin, as shown.
    EmptyBin(Prompt<EmptyRecycleBin>),
    /// Guidance only; nothing runs.
    Guidance,
}

/// Managed state: prepared tools.
#[derive(Debug, Default)]
pub struct PendingTools(pub PendingConsents<PreparedTool>);

/// The confirmation the UI shows before running (`ToolPrompt` in the UI).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolPrompt {
    /// Handle for `tools_run`.
    pub prompt_id: u64,
    /// Title.
    pub title: String,
    /// What it does.
    pub description: String,
    /// The exact command line (empty for the Recycle Bin, which uses the
    /// Shell API).
    pub command_line: String,
    /// How it starts.
    pub launch: Launch,
    /// Output is captured.
    pub captures_output: bool,
    /// Silent options removed from an uninstall command.
    pub removed_flags: Vec<String>,
    /// Unix ms after which the prompt is void.
    pub expires_ms: i64,
    /// For the Recycle Bin: what the consent covers.
    pub recycle_bin: Option<BinCounts>,
}

/// Items and bytes in a Recycle Bin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BinCounts {
    /// Items.
    pub items: u64,
    /// Bytes.
    pub bytes: u64,
}

fn context() -> FeatureResult<ToolContext> {
    let known = strata_win::known::known_folders()
        .map_err(|e| FeatureError::new(ErrorKind::Io, format!("known folders: {e}")))?;
    ToolContext::from_known(&known)
        .ok_or_else(|| FeatureError::unavailable("the Windows folder is unknown"))
}

fn tool_error(e: &ToolError) -> FeatureError {
    FeatureError::with_detail(ErrorKind::Tool, e.to_string(), e)
}

/// Builds the command for `action` with the machine's `{WINDIR}`.
///
/// # Errors
///
/// Unknown Windows folder, or a malformed uninstall string.
pub fn build_spec(action: &ToolAction) -> FeatureResult<CommandSpec> {
    tools::build(&context()?, action).map_err(|e| tool_error(&e))
}

const EMPTY_BIN_TITLE: &str = "Empty Recycle Bin";
const EMPTY_BIN_DESCRIPTION: &str =
    "Permanently deletes everything in the Recycle Bin. This cannot be undone.";

fn pending<R: Runtime>(app: &AppHandle<R>) -> FeatureResult<&PendingConsents<PreparedTool>> {
    app.try_state::<PendingTools>()
        .map(|s| &s.inner().0)
        .ok_or_else(|| FeatureError::internal("tool state missing"))
}

/// The catalog's uninstall command for app `app_id`.
fn uninstall_action<R: Runtime>(app: &AppHandle<R>, app_id: &str) -> FeatureResult<ToolAction> {
    let state = app
        .try_state::<std::sync::Arc<crate::state::AppState>>()
        .ok_or_else(|| FeatureError::internal("app state missing"))?;
    let engine = state
        .engine
        .get()
        .ok_or_else(|| FeatureError::unavailable("the app catalog is still loading"))?;
    let catalog = engine
        .catalog
        .get()
        .ok_or_else(|| FeatureError::unavailable("the app catalog is still loading"))?;
    let entry = catalog
        .apps()
        .iter()
        .find(|a| crate::insights::app_id(a) == app_id)
        .ok_or_else(|| FeatureError::not_found("that app is no longer installed"))?;
    let uninstall_string = entry
        .uninstall_string
        .clone()
        .ok_or_else(|| FeatureError::unavailable("this app registered no uninstaller"))?;
    Ok(ToolAction::Uninstall {
        app_name: entry.name.clone(),
        uninstall_string,
    })
}

fn bin_root(drive: Option<&str>) -> FeatureResult<Option<String>> {
    Ok(drive_letter(drive)?.map(|c| format!("{c}:\\")))
}

/// Builds the command and the consent prompt for a tool; nothing runs
/// (`tools_prepare`).
#[tauri::command]
pub async fn tools_prepare<R: Runtime>(
    app: AppHandle<R>,
    action: ToolRequest,
) -> FeatureResult<ToolPrompt> {
    blocking(move || {
        let now = Instant::now();
        let (prepared, prompt) = match &action {
            ToolRequest::EmptyRecycleBin { drive } => {
                let root = bin_root(drive.as_deref())?;
                let p = tools::empty_recycle_bin_prompt(root.as_deref())
                    .map_err(|e| FeatureError::io_err("the Recycle Bin could not be read", &e))?;
                let counts = BinCounts {
                    items: p.action().items,
                    bytes: p.action().bytes,
                };
                let description = p.text();
                (
                    PreparedTool::EmptyBin(p),
                    ToolPrompt {
                        prompt_id: 0,
                        title: EMPTY_BIN_TITLE.into(),
                        description: format!("{EMPTY_BIN_DESCRIPTION}\n\n{description}"),
                        command_line: String::new(),
                        launch: Launch::ShellOpen,
                        captures_output: false,
                        removed_flags: Vec::new(),
                        expires_ms: 0,
                        recycle_bin: Some(counts),
                    },
                )
            }
            other => {
                let action = match other {
                    ToolRequest::DiskCleanup { drive } => ToolAction::DiskCleanup {
                        drive: drive_letter(drive.as_deref())?,
                    },
                    ToolRequest::StorageSenseSettings => ToolAction::StorageSenseSettings,
                    ToolRequest::DismComponentCleanup => ToolAction::DismComponentCleanup,
                    ToolRequest::SystemProtection => ToolAction::SystemProtection,
                    ToolRequest::HibernationGuidance => ToolAction::HibernationGuidance,
                    ToolRequest::CompactOsStatus => ToolAction::CompactOsStatus,
                    ToolRequest::Uninstall { app_id } => uninstall_action(&app, app_id)?,
                    ToolRequest::EmptyRecycleBin { .. } => unreachable!("handled above"),
                };
                let spec = build_spec(&action)?;
                let prompt = ToolPrompt {
                    prompt_id: 0,
                    title: spec.title.clone(),
                    description: spec.description.clone(),
                    command_line: spec.command_line.clone(),
                    launch: spec.launch,
                    captures_output: spec.captures_output,
                    removed_flags: spec.removed_flags.clone(),
                    expires_ms: 0,
                    recycle_bin: None,
                };
                let prepared = if spec.launch == Launch::GuidanceOnly {
                    PreparedTool::Guidance
                } else {
                    PreparedTool::Command(spec)
                };
                (prepared, prompt)
            }
        };
        let (prompt_id, expires_ms) = pending(&app)?.offer_numbered(prepared, now)?;
        Ok(ToolPrompt {
            prompt_id,
            expires_ms,
            ..prompt
        })
    })
    .await
}

/// A line of captured output.
#[derive(Debug, Clone, Serialize)]
pub struct OutputLine {
    /// `stdout` or `stderr`.
    pub stream: &'static str,
    /// The line.
    pub line: String,
}

/// Result of running a tool (`ToolRunResult` in the UI).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRunResult {
    /// Exit code, when the app waited for the process.
    pub exit_code: Option<i64>,
    /// Captured output ("" when not captured).
    pub output: String,
}

/// Runs a prepared tool the user confirmed (`tools_run`), sending captured
/// output lines over `on_output` when it finishes.
#[tauri::command]
pub async fn tools_run<R: Runtime>(
    app: AppHandle<R>,
    prompt_id: u64,
    on_output: Channel<OutputLine>,
) -> FeatureResult<ToolRunResult> {
    let prepared = pending(&app)?.take_numbered(prompt_id, Instant::now())?;
    let store = super::store::handle(&app)?;
    let helper = app
        .try_state::<super::cleanup::CleanupState>()
        .and_then(|s| s.service().privileged());
    let out: ToolOutput = blocking(move || match prepared {
        PreparedTool::Guidance => Err(tool_error(&ToolError::NotRunnable)),
        PreparedTool::EmptyBin(prompt) => {
            let action = prompt.action().clone();
            logged(&store, &bin_log_item(&action), || {
                // This handler is the confirmation click; the consent never
                // leaves this thread.
                tools::empty_recycle_bin(prompt.confirm()).map_err(|e| tool_error(&e))
            })?;
            Ok(ToolOutput {
                exit_code: None,
                output: None,
            })
        }
        PreparedTool::Command(spec) => {
            let run = || -> FeatureResult<ToolOutput> {
                let via_helper = (spec.launch == Launch::Elevated)
                    .then(|| helper.as_ref().and_then(|h| h.run_tool(&spec)))
                    .flatten();
                via_helper
                    .unwrap_or_else(|| tools::run(&spec))
                    .map_err(|e| tool_error(&e))
            };
            if spec.launch == Launch::Elevated {
                logged(&store, &tool_log_item(&spec), run)
            } else {
                run()
            }
        }
    })
    .await?;
    let output = out.output.unwrap_or_default();
    for line in output.lines() {
        let _ = on_output.send(OutputLine {
            stream: "stdout",
            line: line.to_owned(),
        });
    }
    Ok(ToolRunResult {
        exit_code: out.exit_code,
        output,
    })
}

/// Read-only status next to each tool (`ToolsStatus` in the UI).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsStatus {
    /// Recycle Bin contents per drive.
    pub recycle_bins: Vec<DriveBin>,
    /// `compact`, `not_compact` or `unknown`.
    pub compact_os: &'static str,
    /// Hibernation state.
    pub hibernation: Hibernation,
    /// Shadow-copy storage in use on the system volume.
    pub shadow_storage_bytes: Option<u64>,
}

/// One drive's Recycle Bin.
#[derive(Debug, Clone, Serialize)]
pub struct DriveBin {
    /// Drive (`C:`).
    pub drive: String,
    /// Items.
    pub items: u64,
    /// Bytes.
    pub bytes: u64,
}

/// Hibernation facts.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hibernation {
    /// `hiberfil.sys` exists on the system drive.
    pub enabled: Option<bool>,
    /// Its size.
    pub hiberfil_bytes: Option<u64>,
}

fn system_drive() -> String {
    std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())
}

/// Recycle Bin bytes and items over every drive.
#[must_use]
pub fn recycle_bin_totals() -> (u64, u64) {
    tools::empty_recycle_bin_prompt(None)
        .map(|p| (p.action().bytes, p.action().items))
        .unwrap_or((0, 0))
}

/// Recycle Bin contents per drive, hibernation file and shadow storage
/// (`tools_status`). Read-only.
#[tauri::command]
pub async fn tools_status() -> FeatureResult<ToolsStatus> {
    blocking(|| {
        let opts = strata_win::volume::DiscoveryOptions {
            include_network: false,
            query_bitlocker: false,
        };
        let vols = strata_win::volume::discover_volumes(opts).unwrap_or_default();
        let mut recycle_bins = Vec::new();
        let mut shadow = None;
        let sys = system_drive().to_ascii_uppercase();
        let report = strata_win::shadow::shadow_storage();
        for v in vols.iter().filter(|v| v.ready) {
            let Some(root) = v.root_path().map(|p| p.display().to_string()) else {
                continue;
            };
            let drive = root.trim_end_matches('\\').to_owned();
            if drive.len() == 2
                && let Ok(p) = tools::empty_recycle_bin_prompt(Some(&root))
            {
                recycle_bins.push(DriveBin {
                    drive: drive.clone(),
                    items: p.action().items,
                    bytes: p.action().bytes,
                });
            }
            if drive.eq_ignore_ascii_case(&sys)
                && let Some(g) = v.guid_path.as_deref()
            {
                shadow = report.for_volume(g).map(|s| s.used_bytes);
            }
        }
        // NOTE: `hiberfil.sys` is a hidden system file; reading its metadata
        // needs no access to its contents.
        let hiber = std::fs::metadata(format!("{sys}\\hiberfil.sys"));
        Ok(ToolsStatus {
            recycle_bins,
            compact_os: "unknown",
            hibernation: match hiber {
                Ok(m) => Hibernation {
                    enabled: Some(true),
                    hiberfil_bytes: Some(m.len()),
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Hibernation {
                    enabled: Some(false),
                    hiberfil_bytes: None,
                },
                Err(_) => Hibernation {
                    enabled: None,
                    hiberfil_bytes: None,
                },
            },
            shadow_storage_bytes: shadow,
        })
    })
    .await
}

/// Empties the Recycle Bin of every drive after the backend's **own native
/// confirmation dialog** (the palette's one-click entry point). Returns
/// whether the bin was emptied (`false` when the user cancelled).
#[tauri::command]
pub async fn recycle_bin_empty<R: Runtime>(app: AppHandle<R>) -> FeatureResult<bool> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        let prompt = tools::empty_recycle_bin_prompt(None)
            .map_err(|e| FeatureError::io_err("the Recycle Bin could not be read", &e))?;
        if prompt.action().items == 0 {
            return Ok(false);
        }
        let mut dialog = app
            .dialog()
            .message(prompt.text())
            .title(EMPTY_BIN_TITLE)
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                EMPTY_BIN_TITLE.into(),
                "Cancel".into(),
            ));
        if let Some(w) = app.get_webview_window("main") {
            dialog = dialog.parent(&w);
        }
        if !dialog.blocking_show() {
            return Ok(false);
        }
        let action = prompt.action().clone();
        logged(&store, &bin_log_item(&action), || {
            tools::empty_recycle_bin(prompt.confirm()).map_err(|e| tool_error(&e))
        })?;
        Ok(true)
    })
    .await
}

fn tool_log_item(spec: &CommandSpec) -> PlannedItem {
    PlannedItem {
        path: spec.command_line.clone(),
        volume: VolumeKey {
            serial: 0,
            guid_path: String::new(),
        },
        file_ref: FileRef(0),
        size: 0,
        mtime: FileTime(0),
        method: DeleteMethod::Tool,
        tier: Safety::Careful,
        rule_id: Some(spec.title.clone()),
    }
}

fn bin_log_item(action: &EmptyRecycleBin) -> PlannedItem {
    PlannedItem {
        path: action.root.clone().map_or_else(
            || "Recycle Bin (all drives)".into(),
            |r| format!("Recycle Bin ({r})"),
        ),
        size: action.bytes,
        rule_id: Some(EMPTY_BIN_TITLE.into()),
        ..tool_log_item(&CommandSpec {
            title: String::new(),
            description: String::new(),
            program: String::new(),
            args: Vec::new(),
            command_line: String::new(),
            launch: Launch::Process,
            captures_output: false,
            removed_flags: Vec::new(),
        })
    }
}

/// Writes the tool action ahead, runs it, records the outcome.
fn logged<T>(
    store: &Store,
    item: &PlannedItem,
    run: impl FnOnce() -> FeatureResult<T>,
) -> FeatureResult<T> {
    let id = store.begin_action(ActionKind::Tool, std::slice::from_ref(item))?;
    let result = run();
    let outcome = match &result {
        Ok(_) => ItemOutcome {
            result: ItemResult::Done,
            error: None,
            restore: None,
        },
        Err(e) => ItemOutcome {
            result: ItemResult::Failed,
            error: Some(e.message.clone()),
            restore: None,
        },
    };
    let _ = store.complete_item(id, 0, &outcome);
    let status = if result.is_ok() {
        ActionStatus::Completed
    } else {
        ActionStatus::Failed
    };
    let _ = store.finish_action(id, status);
    result
}

// -----------------------------------------------------------------------------
// Recycle Bin facts
// -----------------------------------------------------------------------------

/// Recycle Bin state of one volume.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecycleBinVolume {
    /// Volume id (GUID path), as in `list_volumes`.
    pub volume_id: String,
    /// Mount point used for the query.
    pub mount_point: String,
    /// Whether deletes on this volume can go to the Recycle Bin.
    pub supported: bool,
    /// Why not, when unsupported.
    pub reason: Option<RecycleUnavailable>,
    /// Plain-language reason.
    pub reason_text: Option<String>,
    /// Configured maximum size in bytes, when Windows recorded one.
    pub capacity_bytes: Option<u64>,
    /// Bytes in this volume's bin.
    pub bytes: u64,
    /// Items in this volume's bin.
    pub items: u64,
}

/// Response of `recycle_bin_info`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecycleBinInfo {
    /// Per volume.
    pub volumes: Vec<RecycleBinVolume>,
    /// Bytes in every bin.
    pub total_bytes: u64,
    /// Items in every bin.
    pub total_items: u64,
}

fn volume_bin(volume_id: String, root: &str) -> RecycleBinVolume {
    let support = strata_clean::canon::CanonicalPath::parse(root)
        .ok()
        .and_then(|c| strata_clean::volume::volume_info(&c).ok())
        .map_or(
            RecycleBinSupport::Unavailable {
                reason: RecycleUnavailable::UnknownVolume,
            },
            |v| v.recycle_bin,
        );
    match support {
        RecycleBinSupport::Available { capacity, used } => {
            let items = tools::empty_recycle_bin_prompt(Some(root))
                .map(|p| p.action().items)
                .unwrap_or(0);
            RecycleBinVolume {
                volume_id,
                mount_point: root.to_string(),
                supported: true,
                reason: None,
                reason_text: None,
                capacity_bytes: capacity,
                bytes: used,
                items,
            }
        }
        RecycleBinSupport::Unavailable { reason } => RecycleBinVolume {
            volume_id,
            mount_point: root.to_string(),
            supported: false,
            reason: Some(reason),
            reason_text: Some(reason.describe().to_string()),
            capacity_bytes: None,
            bytes: 0,
            items: 0,
        },
    }
}

/// Recycle Bin size and support for every local volume.
#[tauri::command]
pub async fn recycle_bin_info() -> FeatureResult<RecycleBinInfo> {
    blocking(|| {
        let opts = strata_win::volume::DiscoveryOptions {
            include_network: false,
            query_bitlocker: false,
        };
        let vols = strata_win::volume::discover_volumes(opts)
            .map_err(|e| FeatureError::new(ErrorKind::Io, format!("volumes: {e}")))?;
        let volumes = vols
            .iter()
            .filter(|v| v.ready)
            .filter_map(|v| {
                let root = v.root_path()?.display().to_string();
                Some(volume_bin(v.id(), &root))
            })
            .collect();
        let (total_bytes, total_items) = tools::empty_recycle_bin_prompt(None)
            .map(|p| (p.action().bytes, p.action().items))
            .unwrap_or((0, 0));
        Ok(RecycleBinInfo {
            volumes,
            total_bytes,
            total_items,
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_deserialize_from_the_ui_shape() {
        let r: ToolRequest =
            serde_json::from_str(r#"{"kind":"empty_recycle_bin","drive":null}"#).unwrap();
        assert_eq!(r, ToolRequest::EmptyRecycleBin { drive: None });
        let r: ToolRequest =
            serde_json::from_str(r#"{"kind":"disk_cleanup","drive":"C:"}"#).unwrap();
        assert_eq!(
            r,
            ToolRequest::DiskCleanup {
                drive: Some("C:".into())
            }
        );
        let r: ToolRequest =
            serde_json::from_str(r#"{"kind":"uninstall","appId":"Example"}"#).unwrap();
        assert_eq!(
            r,
            ToolRequest::Uninstall {
                app_id: "Example".into()
            }
        );
        // The webview can never hand over its own command.
        assert!(
            serde_json::from_str::<ToolRequest>(
                r#"{"kind":"uninstall","app_name":"x","uninstall_string":"evil.exe"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn only_drives_are_accepted() {
        assert_eq!(drive_letter(Some("c")).unwrap(), Some('C'));
        assert_eq!(drive_letter(Some(r"d:\")).unwrap(), Some('D'));
        assert_eq!(drive_letter(None).unwrap(), None);
        assert!(drive_letter(Some(r"C:\Windows")).is_err());
        assert!(drive_letter(Some(r"\\srv\share")).is_err());
        assert_eq!(bin_root(Some("e:")).unwrap().as_deref(), Some(r"E:\"));
    }

    #[test]
    fn commands_are_absolute_and_flags_shown() {
        let ctx = ToolContext {
            windir: r"C:\Windows".into(),
        };
        let spec = tools::build(&ctx, &ToolAction::DismComponentCleanup).unwrap();
        assert_eq!(
            spec.command_line,
            r"C:\Windows\System32\Dism.exe /Online /Cleanup-Image /StartComponentCleanup"
        );
        assert_eq!(spec.launch, Launch::Elevated);
        let spec = tools::build(
            &ctx,
            &ToolAction::Uninstall {
                app_name: "App".into(),
                uninstall_string: r#""C:\A\u.exe" /S"#.into(),
            },
        )
        .unwrap();
        assert_eq!(spec.removed_flags, ["/S"]);
    }

    #[test]
    fn tool_actions_are_logged_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let item = bin_log_item(&EmptyRecycleBin {
            root: Some(r"C:\".into()),
            items: 3,
            bytes: 30,
        });
        let r: FeatureResult<()> = logged(&store, &item, || {
            // Inside the run the action is already written ahead.
            assert_eq!(store.recover_incomplete().unwrap().len(), 1);
            Err(FeatureError::internal("simulated"))
        });
        assert!(r.is_err());
        let a = store.action_history(1, None).unwrap();
        assert_eq!(a[0].kind, ActionKind::Tool);
        assert_eq!(a[0].status, ActionStatus::Failed);
        assert!(store.recover_incomplete().unwrap().is_empty());
    }

    #[test]
    fn recycle_bin_facts_for_the_system_drive() {
        let root = format!(
            "{}\\",
            std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())
        );
        let v = volume_bin("test".into(), &root);
        assert!(v.supported, "{v:?}");
        assert!(v.reason.is_none());
    }
}
