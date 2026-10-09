//! Built-in Windows tools and Recycle Bin facts (SPEC §15.6, §15.2 step 4).
//!
//! Every tool goes through a prepare/run round trip: `tools_prepare` builds
//! the exact command (`strata_clean::tools::CommandSpec`) **on the backend**
//! and returns it with a single-use token; the UI shows the command line and
//! description; `tools_run` executes exactly the stored spec. The UI can
//! never supply a program or arguments. Emptying the Recycle Bin is the same
//! round trip, with the prompt built from the bin's current item count and
//! size and redeemed as a `Consent`.
//!
//! Destructive tools (DISM component cleanup, emptying the bin) are written
//! ahead to the undo/audit log as `tool` actions.

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
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use super::consent::{ConsentTicket, PendingConsents};
use super::error::{ErrorKind, FeatureError, FeatureResult, blocking};

/// A tool the UI may ask for. Uninstallers are not here: their command comes
/// from the installed-apps catalog, so the backend prepares them with
/// [`prepare_tool`] and never takes an uninstall string from the webview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolRequest {
    /// Windows Disk Cleanup, optionally for one drive letter.
    DiskCleanup {
        /// Drive letter.
        drive: Option<char>,
    },
    /// Storage Sense settings page.
    StorageSenseSettings,
    /// DISM component store cleanup (elevated, output captured).
    DismComponentCleanup,
    /// System Protection dialog.
    SystemProtection,
    /// Hibernation file guidance (never executed).
    HibernationGuidance,
    /// `compact /compactos:query` (read-only).
    CompactOsStatus,
    /// Empty the Recycle Bin of one drive root (`C:\`) or of every drive.
    EmptyRecycleBin {
        /// Drive root; `None` for every drive.
        root: Option<String>,
    },
}

impl ToolRequest {
    fn action(&self) -> Option<ToolAction> {
        Some(match self {
            Self::DiskCleanup { drive } => ToolAction::DiskCleanup { drive: *drive },
            Self::StorageSenseSettings => ToolAction::StorageSenseSettings,
            Self::DismComponentCleanup => ToolAction::DismComponentCleanup,
            Self::SystemProtection => ToolAction::SystemProtection,
            Self::HibernationGuidance => ToolAction::HibernationGuidance,
            Self::CompactOsStatus => ToolAction::CompactOsStatus,
            Self::EmptyRecycleBin { .. } => return None,
        })
    }

    /// Every tool in the order the UI lists them.
    #[must_use]
    pub fn all() -> Vec<Self> {
        vec![
            Self::EmptyRecycleBin { root: None },
            Self::DiskCleanup { drive: None },
            Self::StorageSenseSettings,
            Self::DismComponentCleanup,
            Self::SystemProtection,
            Self::HibernationGuidance,
            Self::CompactOsStatus,
        ]
    }
}

/// A prepared tool waiting for confirmation.
#[derive(Debug)]
pub enum PreparedTool {
    /// A command to run exactly as shown.
    Command(CommandSpec),
    /// Emptying the Recycle Bin, as shown.
    EmptyBin(Prompt<EmptyRecycleBin>),
}

/// Managed state: prepared tools.
#[derive(Debug, Default)]
pub struct PendingTools(pub PendingConsents<PreparedTool>);

/// One entry of `tools_list`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    /// What to pass to `tools_prepare`.
    pub request: ToolRequest,
    /// Title.
    pub title: String,
    /// What it does.
    pub description: String,
    /// The exact command line (empty for the Recycle Bin, which uses the
    /// Shell API).
    pub command_line: String,
    /// How it starts: `process`, `shell_open`, `elevated`, `guidance_only`.
    pub launch: Option<Launch>,
    /// Whether output is captured and returned by `tools_run`.
    pub captures_output: bool,
}

/// Response of `tools_prepare`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedToolDto {
    /// Token and confirmation text.
    #[serde(flatten)]
    pub ticket: ConsentTicket,
    /// The exact command, for command tools.
    pub spec: Option<CommandSpec>,
}

fn context() -> FeatureResult<ToolContext> {
    let known = strata_win::known::known_folders()
        .map_err(|e| FeatureError::new(ErrorKind::Io, format!("known folders: {e}")))?;
    ToolContext::from_known(&known)
        .ok_or_else(|| FeatureError::new(ErrorKind::Unsupported, "the Windows folder is unknown"))
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

/// Every built-in tool with its exact command.
#[tauri::command]
pub async fn tools_list() -> FeatureResult<Vec<ToolInfo>> {
    blocking(|| {
        let ctx = context()?;
        ToolRequest::all()
            .into_iter()
            .map(|request| match request.action() {
                None => Ok(ToolInfo {
                    request,
                    title: EMPTY_BIN_TITLE.into(),
                    description: EMPTY_BIN_DESCRIPTION.into(),
                    command_line: String::new(),
                    launch: None,
                    captures_output: false,
                }),
                Some(action) => {
                    let spec = tools::build(&ctx, &action).map_err(|e| tool_error(&e))?;
                    Ok(ToolInfo {
                        request,
                        title: spec.title,
                        description: spec.description,
                        command_line: spec.command_line,
                        launch: Some(spec.launch),
                        captures_output: spec.captures_output,
                    })
                }
            })
            .collect()
    })
    .await
}

fn pending<R: Runtime>(app: &AppHandle<R>) -> FeatureResult<&PendingConsents<PreparedTool>> {
    app.try_state::<PendingTools>()
        .map(|s| &s.inner().0)
        .ok_or_else(|| FeatureError::internal("tool state missing"))
}

/// The confirmation text for a command: what it does, then the exact
/// command line.
#[must_use]
pub fn command_text(spec: &CommandSpec) -> String {
    let mut text = format!("{}\n\n{}", spec.description, spec.command_line);
    if !spec.removed_flags.is_empty() {
        text.push_str(&format!(
            "\n\nRemoved silent options: {}",
            spec.removed_flags.join(" ")
        ));
    }
    text
}

/// Prepares any tool, including uninstallers from the apps catalog (for
/// the backend track; the webview cannot reach this with its own strings).
///
/// # Errors
///
/// As [`build_spec`], or a guidance-only action (nothing to run).
pub fn prepare_tool<R: Runtime>(
    app: &AppHandle<R>,
    action: &ToolAction,
) -> FeatureResult<PreparedToolDto> {
    let spec = build_spec(action)?;
    if spec.launch == Launch::GuidanceOnly {
        return Err(tool_error(&ToolError::NotRunnable));
    }
    let text = command_text(&spec);
    let ticket = pending(app)?.offer(PreparedTool::Command(spec.clone()), text, Instant::now())?;
    Ok(PreparedToolDto {
        ticket,
        spec: Some(spec),
    })
}

/// Prepares a tool: returns the exact command and a token for `tools_run`.
#[tauri::command]
pub async fn tools_prepare<R: Runtime>(
    app: AppHandle<R>,
    request: ToolRequest,
) -> FeatureResult<PreparedToolDto> {
    blocking(move || match &request {
        ToolRequest::EmptyRecycleBin { root } => {
            let root = root.as_deref().map(normalize_root).transpose()?;
            let prompt = tools::empty_recycle_bin_prompt(root.as_deref())
                .map_err(|e| FeatureError::io("the Recycle Bin could not be read", &e))?;
            let text = prompt.text();
            let ticket =
                pending(&app)?.offer(PreparedTool::EmptyBin(prompt), text, Instant::now())?;
            Ok(PreparedToolDto { ticket, spec: None })
        }
        other => {
            let action = other
                .action()
                .ok_or_else(|| FeatureError::internal("no action"))?;
            prepare_tool(&app, &action)
        }
    })
    .await
}

/// Drive roots only (`C:\`); anything else is refused.
fn normalize_root(root: &str) -> FeatureResult<String> {
    let mut chars = root.chars();
    match (chars.next(), chars.next(), chars.as_str()) {
        (Some(d), Some(':'), "" | "\\" | "/") if d.is_ascii_alphabetic() => {
            Ok(format!("{}:\\", d.to_ascii_uppercase()))
        }
        _ => Err(FeatureError::invalid(format!("{root} is not a drive root"))),
    }
}

/// Runs a prepared tool the user confirmed. Returns captured output for
/// tools that capture it.
#[tauri::command]
pub async fn tools_run<R: Runtime>(app: AppHandle<R>, token: String) -> FeatureResult<ToolOutput> {
    let prepared = pending(&app)?.take(&token, Instant::now())?;
    let store = super::store::handle(&app)?;
    let helper = app
        .try_state::<super::cleanup::CleanupState>()
        .and_then(|s| s.service().privileged());
    blocking(move || match prepared {
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
    .await
}

/// The user dismissed the confirmation.
#[tauri::command]
pub fn tools_cancel<R: Runtime>(app: AppHandle<R>, token: String) -> FeatureResult<()> {
    pending(&app)?.cancel(&token);
    Ok(())
}

/// Empties the Recycle Bin of every drive after the backend's **own native
/// confirmation dialog** (the palette's one-click entry point). Returns
/// whether the bin was emptied (`false` when the user cancelled).
#[tauri::command]
pub async fn recycle_bin_empty<R: Runtime>(app: AppHandle<R>) -> FeatureResult<bool> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        let prompt = tools::empty_recycle_bin_prompt(None)
            .map_err(|e| FeatureError::io("the Recycle Bin could not be read", &e))?;
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
    fn requests_map_to_tool_actions_with_absolute_commands() {
        let ctx = ToolContext {
            windir: r"C:\Windows".into(),
        };
        let spec =
            tools::build(&ctx, &ToolRequest::DismComponentCleanup.action().unwrap()).unwrap();
        assert_eq!(
            spec.command_line,
            r"C:\Windows\System32\Dism.exe /Online /Cleanup-Image /StartComponentCleanup"
        );
        assert_eq!(spec.launch, Launch::Elevated);
        let text = command_text(&spec);
        assert!(text.ends_with(&spec.command_line), "{text}");
        let spec = tools::build(
            &ctx,
            &ToolRequest::DiskCleanup { drive: Some('d') }
                .action()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(spec.command_line, r"C:\Windows\System32\cleanmgr.exe /d D");
        assert!(
            ToolRequest::EmptyRecycleBin { root: None }
                .action()
                .is_none()
        );
        assert_eq!(ToolRequest::all().len(), 7);
    }

    #[test]
    fn requests_deserialize_from_the_ui_shape() {
        let r: ToolRequest =
            serde_json::from_str(r#"{"kind":"empty_recycle_bin","root":"C:\\"}"#).unwrap();
        assert_eq!(
            r,
            ToolRequest::EmptyRecycleBin {
                root: Some(r"C:\".into())
            }
        );
        let r: ToolRequest =
            serde_json::from_str(r#"{"kind":"disk_cleanup","drive":"C"}"#).unwrap();
        assert_eq!(r, ToolRequest::DiskCleanup { drive: Some('C') });
        assert!(
            serde_json::from_str::<ToolRequest>(
                r#"{"kind":"uninstall","app_name":"x","uninstall_string":"evil.exe"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn removed_silent_flags_are_shown() {
        let ctx = ToolContext {
            windir: r"C:\Windows".into(),
        };
        let spec = tools::build(
            &ctx,
            &ToolAction::Uninstall {
                app_name: "App".into(),
                uninstall_string: r#""C:\A\u.exe" /S"#.into(),
            },
        )
        .unwrap();
        assert!(command_text(&spec).contains("Removed silent options: /S"));
    }

    #[test]
    fn only_drive_roots_are_accepted() {
        assert_eq!(normalize_root("c:").unwrap(), r"C:\");
        assert_eq!(normalize_root(r"D:\").unwrap(), r"D:\");
        assert!(normalize_root(r"C:\Windows").is_err());
        assert!(normalize_root(r"\\srv\share").is_err());
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
