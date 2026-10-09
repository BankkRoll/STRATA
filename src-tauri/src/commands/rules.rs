//! Rule tooling (`ui/src/lib/settings.ts`): list, open folder, reload and
//! "why is this classified as X?".

use std::sync::Arc;

use serde::Serialize;
use strata_classify::{Action, RuleSource, StdFsProbe};
use strata_core::Safety;
use tauri::{AppHandle, Manager, State};

use super::blocking;
use crate::classify::{Engine, classify_index, ext_slots};
use crate::error::{CmdResult, CommandError};
use crate::state::AppState;

/// A rule as listed in settings (`RuleInfo` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleInfo {
    /// Rule id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Pack id.
    pub pack: String,
    /// `builtin` or `user`.
    pub source: &'static str,
    /// Category id.
    pub category: u16,
    /// Tier.
    pub safety: Safety,
    /// Explanation.
    pub explain: String,
    /// `delete`, `open_tool` or `info_only`.
    pub action: Action,
    /// Attribution label.
    pub app: Option<String>,
    /// Re-created by its app.
    pub regenerable: bool,
    /// Id of the user rule that overrides this built-in.
    pub overridden_by: Option<String>,
}

fn rule_info(r: &strata_classify::Rule, overridden_by: Option<String>) -> RuleInfo {
    RuleInfo {
        id: r.id.clone(),
        name: r.name.clone(),
        pack: r.pack.clone(),
        source: match r.source {
            RuleSource::Builtin => "builtin",
            RuleSource::User(_) => "user",
        },
        category: r.category as u16,
        safety: r.safety,
        explain: r.explain.clone(),
        action: r.action,
        app: r.app.clone(),
        regenerable: r.regenerable,
        overridden_by,
    }
}

fn engine(state: &AppState) -> CmdResult<Arc<Engine>> {
    state
        .engine
        .get()
        .ok_or_else(|| CommandError::unavailable("rules are still loading"))
}

/// Every loaded rule, built-in and user (`rules_list`).
///
/// # Errors
///
/// `unavailable` while rules load.
#[tauri::command]
pub fn rules_list(state: State<'_, Arc<AppState>>) -> CmdResult<Vec<RuleInfo>> {
    let e = engine(&state)?;
    let rules = e.classifier.rules();
    Ok(rules
        .iter()
        .map(|r| {
            // A user rule with the same id replaces the built-in one.
            let over = (r.is_builtin())
                .then(|| {
                    rules
                        .iter()
                        .find(|u| !u.is_builtin() && u.id == r.id)
                        .map(|u| u.id.clone())
                })
                .flatten();
            rule_info(r, over)
        })
        .collect())
}

/// The user rules folder, created if missing.
fn user_rules_dir(app: &AppHandle, state: &AppState) -> CmdResult<std::path::PathBuf> {
    let data = app.path().app_local_data_dir().ok();
    let dir = crate::rules_dir(state, data.as_deref()).ok_or_else(|| {
        CommandError::unavailable("user rules are turned off in Settings > Rules")
    })?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| CommandError::io_err("could not create the rules folder", &e))?;
    Ok(dir)
}

/// Opens the user rules folder in Explorer (`rules_open_folder`).
///
/// # Errors
///
/// User rules off, or the folder could not be created or opened.
#[tauri::command]
pub fn rules_open_folder(app: AppHandle, state: State<'_, Arc<AppState>>) -> CmdResult<()> {
    let dir = user_rules_dir(&app, &state)?;
    crate::shell::open(&dir).map_err(CommandError::io)
}

/// One problem found while loading user rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleProblem {
    /// File (or pack) it came from.
    pub file: String,
    /// What is wrong.
    pub message: String,
}

/// Result of `rules_reload` (`RulesReload` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RulesReload {
    /// Built-in rules loaded.
    pub builtin: u64,
    /// User rules loaded.
    pub user: u64,
    /// Problems in user packs.
    pub problems: Vec<RuleProblem>,
}

/// Reclassifies every indexed volume with the current engine.
pub fn reclassify_all(state: &AppState) {
    let Some(engine) = state.engine.get() else {
        return;
    };
    let ids: Vec<String> = crate::state::lock(&state.sessions)
        .keys()
        .cloned()
        .collect();
    for id in ids {
        let Some(s) = state.existing_session(&id) else {
            continue;
        };
        let mut g = s
            .data
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(data) = g.as_mut() {
            let slots = ext_slots(&data.index);
            let root = data.root_path.clone();
            let c = classify_index(&engine, &mut data.index, &root, &slots);
            data.classes = c.classes;
            data.keys = c.keys;
            data.after_change(None);
        }
    }
}

/// Reloads the rule packs (built-in plus the user folder), swaps in the new
/// classifier and reclassifies every index (`rules_reload`).
///
/// # Errors
///
/// When the built-in packs fail to compile (a bug).
#[tauri::command]
pub async fn rules_reload(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> CmdResult<RulesReload> {
    let st = state.inner().clone();
    blocking(move || {
        let data = app.path().app_local_data_dir().ok();
        let dir = crate::rules_dir(&st, data.as_deref());
        let kf = strata_win::known::known_folders().unwrap_or_default();
        let engine = Engine::new(&kf, dir.as_deref()).map_err(CommandError::internal)?;
        engine.load_catalog(&kf);
        let rules = engine.classifier.rules();
        let builtin = rules.iter().filter(|r| r.is_builtin()).count() as u64;
        let problems = crate::state::lock(&engine.warnings)
            .iter()
            .map(|w| RuleProblem {
                file: dir
                    .as_ref()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default(),
                message: w.clone(),
            })
            .collect();
        let report = RulesReload {
            builtin,
            user: rules.len() as u64 - builtin,
            problems,
        };
        st.engine.set(Ok(Arc::new(engine)));
        reclassify_all(&st);
        crate::jobs::emit_volumes(&app, &st);
        Ok(report)
    })
    .await
}

/// One ancestor's classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainStep {
    /// Path.
    pub path: String,
    /// Deciding rule.
    pub rule_id: Option<String>,
    /// Category id.
    pub category: u16,
    /// Tier.
    pub safety: Safety,
}

/// The classification result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainResult {
    /// Category id.
    pub category: u16,
    /// Tier.
    pub safety: Safety,
    /// Deciding rule.
    pub rule_id: Option<String>,
    /// Re-created by its app.
    pub regenerable: bool,
}

/// "Why is this classified as X?" (`Explanation` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Explanation {
    /// Path.
    pub path: String,
    /// Result.
    pub result: ExplainResult,
    /// The deciding rule.
    pub rule: Option<RuleInfo>,
    /// Where it was inherited from.
    pub origin_path: Option<String>,
    /// Each ancestor from the root down.
    pub steps: Vec<ExplainStep>,
    /// Candidate rules considered, in order.
    pub trace: Vec<String>,
}

/// Explains a path's classification; reads metadata only (`rules_explain`).
///
/// # Errors
///
/// `unavailable` while rules load.
#[tauri::command]
pub async fn rules_explain(
    state: State<'_, Arc<AppState>>,
    path: String,
) -> CmdResult<Explanation> {
    let st = state.inner().clone();
    blocking(move || {
        let e = engine(&st)?;
        let c = &e.classifier;
        let x = c.explain(&path, &StdFsProbe);
        let rule_id = x.result.rule.map(|r| c.rule(r).id.clone());
        let rule = x.result.rule.map(|r| rule_info(c.rule(r), None));
        Ok(Explanation {
            path: x.path.clone(),
            result: ExplainResult {
                category: x.result.category as u16,
                safety: x.result.safety,
                regenerable: x.result.rule.is_some_and(|r| c.rule(r).regenerable),
                rule_id,
            },
            rule,
            origin_path: x.origin_path.clone(),
            steps: x
                .steps
                .iter()
                .map(|s| ExplainStep {
                    path: s.path.clone(),
                    rule_id: s.rule_id.clone(),
                    category: s.classification.category as u16,
                    safety: s.classification.safety,
                })
                .collect(),
            trace: x.to_text(c).lines().map(str::to_owned).collect(),
        })
    })
    .await
}
