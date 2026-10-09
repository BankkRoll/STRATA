//! `app_capabilities`: which registered commands work right now.
//!
//! Every command in [`crate::COMMAND_NAMES`] has a [`Requirement`]; the UI
//! enables exactly the commands whose requirement holds, so a feature whose
//! store, undo log or helper is missing shows its designed "unavailable"
//! state instead of failing on use.

use std::sync::Arc;

use tauri::{AppHandle, Manager, Runtime};

use crate::features::cleanup::{CleanupState, Readiness};
use crate::state::AppState;

/// What a command needs to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Nothing beyond the app itself.
    Always,
    /// The settings/history store opened.
    Store,
    /// The store opened and cleanup is not disabled (it may still be
    /// recovering, in which case commands answer `not_ready`).
    Cleanup,
    /// `strata-helper.exe` ships next to the app.
    HelperBinary,
    /// The store opened and the helper binary ships with the app.
    StoreAndHelperBinary,
    /// A helper is connected (elevated deletes, delete on reboot).
    HelperConnected,
    /// The updater is configured (release builds with a public key).
    Updater,
}

/// The requirement of a command, `None` for names that are not commands.
#[must_use]
pub fn requirement(name: &str) -> Option<Requirement> {
    use Requirement::{
        Always, Cleanup, HelperBinary, HelperConnected, Store, StoreAndHelperBinary, Updater,
    };
    Some(match name {
        "app_info"
        | "app_capabilities"
        | "about_licenses"
        | "report_issue"
        | "open_url"
        | "app_launch_request"
        | "startup_status"
        | "store_health"
        | "store_reset_history"
        | "store_reset_state"
        | "list_volumes"
        | "helper_status"
        | "scan_start"
        | "scan_cancel"
        | "layout_open"
        | "layout_request"
        | "layout_close"
        | "entry_info"
        | "entry_path"
        | "entry_detail"
        | "list_children"
        | "apps_brief"
        | "entry_action"
        | "search_open"
        | "search_query"
        | "search_close"
        | "insights_largest"
        | "insights_file_types"
        | "insights_categories"
        | "apps_footprint"
        | "apps_orphans"
        | "recommendations_list"
        | "recommendations_preview"
        | "rules_list"
        | "rules_open_folder"
        | "rules_reload"
        | "rules_explain"
        | "cleanup_status"
        | "locks_query"
        | "tools_status"
        | "tools_prepare"
        | "recycle_bin_info"
        | "dupes_status"
        | "dupes_start"
        | "dupes_cancel"
        | "dupes_groups"
        | "dupes_hardlink_prompt"
        | "activity_status" => Always,
        "updates_check" | "updates_restart" => Updater,
        "data_clear"
        | "history_snapshots"
        | "history_usage"
        | "history_diff"
        | "history_dir_series"
        | "history_since_last_scan"
        | "settings_load"
        | "settings_save"
        | "settings_export"
        | "settings_import"
        | "cleanup_history"
        | "cleanup_restore"
        | "tools_run"
        | "recycle_bin_empty"
        | "dupes_hardlink"
        | "activity_top"
        | "activity_dir_writers"
        | "activity_clear" => Store,
        "cleanup_queue_list"
        | "cleanup_queue_add"
        | "cleanup_queue_remove"
        | "cleanup_queue_clear"
        | "cleanup_plan"
        | "cleanup_preflight"
        | "cleanup_close_prompt"
        | "cleanup_close_app"
        | "cleanup_execute"
        | "cleanup_cancel"
        | "cleanup_retry_plan"
        | "apps_queue_caches"
        | "recommendations_queue"
        | "dupes_queue" => Cleanup,
        "helper_elevate" => HelperBinary,
        "helper_service_status"
        | "helper_service_install"
        | "helper_service_uninstall"
        | "activity_set_enabled" => StoreAndHelperBinary,
        "cleanup_execute_elevated" | "cleanup_delete_on_reboot" => HelperConnected,
        _ => return None,
    })
}

/// Facts the requirements are checked against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Facts {
    /// The store opened.
    pub store: bool,
    /// Cleanup is not disabled.
    pub cleanup: bool,
    /// The helper binary ships with the app.
    pub helper_binary: bool,
    /// A helper is connected.
    pub helper_connected: bool,
    /// The updater is configured.
    pub updater: bool,
}

impl Requirement {
    /// Whether the requirement holds.
    #[must_use]
    pub const fn met(self, f: &Facts) -> bool {
        match self {
            Self::Always => true,
            Self::Store => f.store,
            Self::Cleanup => f.store && f.cleanup,
            Self::HelperBinary => f.helper_binary,
            Self::StoreAndHelperBinary => f.store && f.helper_binary,
            Self::HelperConnected => f.helper_connected,
            Self::Updater => f.updater,
        }
    }
}

/// The commands that work under `facts`.
#[must_use]
pub fn available(facts: &Facts) -> Vec<&'static str> {
    crate::COMMAND_NAMES
        .iter()
        .copied()
        .filter(|n| requirement(n).is_some_and(|r| r.met(facts)))
        .collect()
}

/// The facts of the running app.
#[must_use]
pub fn facts<R: Runtime>(app: &AppHandle<R>) -> Facts {
    let store = crate::features::store::handle(app).is_ok();
    let cleanup = app
        .try_state::<CleanupState>()
        .is_some_and(|s| !matches!(s.service().readiness(), Readiness::Unavailable { .. }));
    let state = app.try_state::<Arc<AppState>>();
    Facts {
        store,
        cleanup,
        helper_binary: state.as_ref().is_some_and(|s| s.helper.available()),
        helper_connected: state.as_ref().is_some_and(|s| s.helper.client().is_some()),
        updater: crate::updater::configured(app),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_has_a_requirement() {
        for name in crate::COMMAND_NAMES {
            assert!(requirement(name).is_some(), "{name} has no requirement");
        }
        let mut names = crate::COMMAND_NAMES.to_vec();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "a command is registered twice");
    }

    #[test]
    fn requirements_gate_availability() {
        let none = available(&Facts::default());
        assert!(none.contains(&"app_info") && none.contains(&"list_volumes"));
        assert!(!none.contains(&"settings_load"));
        assert!(!none.contains(&"cleanup_plan"));
        assert!(!none.contains(&"helper_elevate"));
        let all = available(&Facts {
            store: true,
            cleanup: true,
            helper_binary: true,
            helper_connected: true,
            updater: true,
        });
        assert_eq!(all.len(), crate::COMMAND_NAMES.len());
        let store_only = available(&Facts {
            store: true,
            ..Facts::default()
        });
        assert!(store_only.contains(&"settings_save"));
        assert!(!store_only.contains(&"cleanup_execute"));
        assert!(!store_only.contains(&"cleanup_delete_on_reboot"));
    }
}
