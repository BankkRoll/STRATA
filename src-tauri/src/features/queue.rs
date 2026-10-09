//! The cleanup queue, review, pre-flight, execution and undo commands the
//! UI calls (`ui/src/lib/cleanup.ts`).
//!
//! The queue lives here, in the backend: entries are added by index id, and
//! the backend resolves each to its path, classification and a fresh
//! identity read from the file itself, refusing never-tier, never-list,
//! virtual, duplicate and nested entries with a sentence the UI shows
//! verbatim. Plans are built from queue ids and kept by
//! [`CleanupService`](super::cleanup::CleanupService); the UI refers to them
//! by `planId` and sends only its decision (method, confirmations, skipped
//! ids) with each pre-flight and execute.
//!
//! Every DTO here is camelCase; engine errors are flattened to
//! [`CleanErrorInfo`] so the UI never re-implements their wording.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use strata_clean::audit::{DeleteMethod, ItemOutcome};
use strata_clean::flow::{
    Acknowledgements, Decision, ExecutionReport, Plan, PlanWarning, Progress, QueueItem,
};
use strata_clean::locks::{AppKind, CloseOutcome, LockHolder};
use strata_clean::preflight::{ItemVerdict, RecycleFit, Verdict};
use strata_clean::volume::RecycleBinSupport;
use strata_clean::{CleanError, Expected};
use strata_core::{EntryFlags, Safety, SizeMode};
use strata_store::{ActionId, ActionKind, ActionStatus, ItemId, ItemRecord, Store};
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, Runtime};

use super::cleanup::{CleanupService, config_from, is_restorable, restore_item};
use super::error::{ErrorKind, FeatureError, FeatureResult, blocking};
use super::locks::{KnownHolders, PendingClose, PendingCloses};
use crate::state::{AppState, read};

/// Event carrying the full queue after every change.
pub const QUEUE_CHANGED: &str = "cleanup://queue-changed";

// -----------------------------------------------------------------------------
// Queue
// -----------------------------------------------------------------------------

/// Where an entry was queued from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueSource {
    /// Context menu, Delete key, list or largest files.
    Manual,
    /// A "Free up space" recommendation.
    Recommendation,
    /// The duplicate finder.
    Duplicates,
    /// An app's cache locations.
    AppCaches,
}

/// One queued entry (`QueueEntry` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueEntry {
    /// Queue id, stable across plan, pre-flight and execute.
    pub id: u64,
    /// Volume of the entry.
    pub volume_id: String,
    /// Index wire id at the time it was queued.
    pub entry_id: u32,
    /// Full Win32 path.
    pub path: String,
    /// Display name.
    pub name: String,
    /// Directory.
    pub is_dir: bool,
    /// Allocated bytes (subtree for folders).
    pub bytes: u64,
    /// Safety tier.
    pub safety: Safety,
    /// `strata_core::Category` discriminant.
    pub category: u16,
    /// Deciding rule, if any.
    pub rule_id: Option<String>,
    /// Its name.
    pub rule_name: Option<String>,
    /// Why the entry has its tier.
    pub explain: String,
    /// The owning app re-creates it.
    pub regenerable: bool,
    /// Owning app.
    pub app: Option<String>,
    /// Unix ms when queued.
    pub added_ms: i64,
    /// Where it was queued from.
    pub source: QueueSource,
}

/// Why an entry was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// Never-tier classification.
    NeverTier,
    /// The never-list protects it.
    NeverList,
    /// Already in the queue.
    AlreadyQueued,
    /// A queued folder already covers it.
    InsideQueued,
    /// Not in the index or not on disk any more.
    NotFound,
    /// Computed by Strata; no file on disk.
    Virtual,
    /// Its identity cannot be checked before deleting.
    Unverifiable,
}

/// An entry that was not queued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueRefusal {
    /// Index wire id.
    pub entry_id: u32,
    /// Path (empty when unknown).
    pub path: String,
    /// Reason code.
    pub reason: RefusalReason,
    /// Sentence shown verbatim.
    pub message: String,
}

/// Result of adding to the queue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct QueueAddResult {
    /// What was added.
    pub added: Vec<QueueEntry>,
    /// What was refused.
    pub refused: Vec<QueueRefusal>,
}

#[derive(Debug, Clone)]
struct Queued {
    entry: QueueEntry,
    item: QueueItem,
}

#[derive(Debug, Default)]
struct QueueInner {
    items: Vec<Queued>,
    next_id: u64,
    /// Entries as they were when each plan was built.
    plans: HashMap<u64, Vec<QueueEntry>>,
}

/// Managed state: the cleanup queue.
#[derive(Debug, Default)]
pub struct CleanupQueue(Mutex<QueueInner>);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn unix_ms() -> i64 {
    crate::model::unix_ms()
}

/// Case-insensitive path key; `\\?\` and trailing separators ignored.
fn path_key(p: &str) -> String {
    p.trim_start_matches(r"\\?\")
        .trim_end_matches('\\')
        .to_lowercase()
}

fn is_inside(inner: &str, outer: &str) -> bool {
    inner.len() > outer.len()
        && inner.starts_with(outer)
        && inner.as_bytes().get(outer.len()) == Some(&b'\\')
}

/// What the index says about one entry to queue.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Volume id.
    pub volume_id: String,
    /// Wire id.
    pub entry_id: u32,
    /// Full path.
    pub path: String,
    /// Name.
    pub name: String,
    /// Directory.
    pub is_dir: bool,
    /// Allocated bytes.
    pub bytes: u64,
    /// Logical bytes (subtree for folders).
    pub logical: u64,
    /// File reference from the scan.
    pub file_ref: Option<strata_core::FileRef>,
    /// Safety tier.
    pub safety: Safety,
    /// Category id.
    pub category: u16,
    /// Rule id.
    pub rule_id: Option<String>,
    /// Rule name.
    pub rule_name: Option<String>,
    /// Explanation.
    pub explain: String,
    /// Regenerable.
    pub regenerable: bool,
    /// App.
    pub app: Option<String>,
    /// Virtual block.
    pub is_virtual: bool,
}

/// Reads candidates for `ids` from a volume's index; unknown ids come back
/// as refusals.
#[must_use]
pub fn candidates(
    state: &AppState,
    volume_id: &str,
    ids: &[u32],
) -> (Vec<Candidate>, Vec<QueueRefusal>) {
    let gone = |w: u32| QueueRefusal {
        entry_id: w,
        path: String::new(),
        reason: RefusalReason::NotFound,
        message: "That item is no longer in the index; scan again.".into(),
    };
    let Some(session) = state.existing_session(volume_id) else {
        return (Vec::new(), ids.iter().map(|&w| gone(w)).collect());
    };
    let guard = read(&session.data);
    let Some(data) = guard.as_ref() else {
        return (Vec::new(), ids.iter().map(|&w| gone(w)).collect());
    };
    let engine = state.engine.get();
    let mut out = Vec::new();
    let mut refused = Vec::new();
    for &w in ids {
        let Some(id) = data.resolve(w) else {
            refused.push(gone(w));
            continue;
        };
        out.push(candidate(data, engine.as_deref(), volume_id, id));
    }
    (out, refused)
}

/// The candidate for one index entry.
#[must_use]
pub fn candidate(
    data: &crate::model::VolumeData,
    engine: Option<&crate::classify::Engine>,
    volume_id: &str,
    id: strata_index::EntryId,
) -> Candidate {
    let ix = &data.index;
    let class = data.class(id);
    let rule = class.and_then(|c| Some(engine?.classifier.rule(c.rule?)));
    let safety = class.map_or(Safety::Careful, |c| c.safety);
    Candidate {
        volume_id: volume_id.to_owned(),
        entry_id: data.wire(id),
        path: data.path(id),
        name: ix.name_lossy(id),
        is_dir: ix.is_dir(id),
        bytes: ix.size(id, SizeMode::Allocated),
        logical: ix.size(id, SizeMode::Logical),
        file_ref: ix.file_ref(id),
        safety,
        category: (data.key_static(id) & 0xF) as u16,
        rule_id: rule.map(|r| r.id.clone()),
        rule_name: rule.map(|r| r.name.clone()),
        explain: rule.map_or_else(
            || match safety {
                Safety::Never => "Windows, program and NTFS system data is protected.".into(),
                _ => "No rule covers this item; review it before deleting.".into(),
            },
            |r| r.explain.clone(),
        ),
        regenerable: rule.is_some_and(|r| r.regenerable),
        app: engine.and_then(|e| e.apps.name(ix.owner_app(id))),
        is_virtual: ix.flags(id).contains(EntryFlags::VIRTUAL),
    }
}

impl CleanupQueue {
    /// The queue in insertion order.
    #[must_use]
    pub fn list(&self) -> Vec<QueueEntry> {
        lock(&self.0)
            .items
            .iter()
            .map(|q| q.entry.clone())
            .collect()
    }

    /// Adds candidates after checking each on disk with `guard`.
    pub fn add(
        &self,
        guard: &strata_clean::SafetyGuard,
        candidates: Vec<Candidate>,
        source: QueueSource,
    ) -> QueueAddResult {
        let mut result = QueueAddResult::default();
        for c in candidates {
            let refuse = |reason, message: String| QueueRefusal {
                entry_id: c.entry_id,
                path: c.path.clone(),
                reason,
                message,
            };
            if c.is_virtual {
                result.refused.push(refuse(
                    RefusalReason::Virtual,
                    format!(
                        "{} is computed by Strata and has no file to delete.",
                        c.name
                    ),
                ));
                continue;
            }
            if c.safety == Safety::Never {
                result.refused.push(refuse(
                    RefusalReason::NeverTier,
                    format!("{} is protected and is never deleted.", c.path),
                ));
                continue;
            }
            if let Err(refusal) = guard.never_list().check_str(std::ffi::OsStr::new(&c.path)) {
                result
                    .refused
                    .push(refuse(RefusalReason::NeverList, refusal.message()));
                continue;
            }
            let key = path_key(&c.path);
            {
                let g = lock(&self.0);
                if g.items.iter().any(|q| path_key(&q.entry.path) == key) {
                    result.refused.push(refuse(
                        RefusalReason::AlreadyQueued,
                        format!("{} is already in the cleanup queue.", c.name),
                    ));
                    continue;
                }
                if let Some(outer) = g
                    .items
                    .iter()
                    .find(|q| is_inside(&key, &path_key(&q.entry.path)))
                {
                    result.refused.push(refuse(
                        RefusalReason::InsideQueued,
                        format!(
                            "{} is inside {}, which is already queued.",
                            c.name, outer.entry.path
                        ),
                    ));
                    continue;
                }
            }
            // The identity the cleaner re-verifies right before acting is
            // read from the file now, and must still be the file the scan saw.
            let checked = match guard.check_path(&PathBuf::from(&c.path)) {
                Ok(ch) => ch,
                Err(e) => {
                    let reason = match e {
                        CleanError::Refused { .. } => RefusalReason::NeverList,
                        CleanError::NotFound { .. } => RefusalReason::NotFound,
                        _ => RefusalReason::Unverifiable,
                    };
                    result.refused.push(refuse(reason, e.message()));
                    continue;
                }
            };
            if let Some(fr) = c.file_ref
                && !fr.is_synthetic()
                && !checked.facts.identity.matches(fr)
            {
                result.refused.push(refuse(
                    RefusalReason::NotFound,
                    format!(
                        "{} changed since the scan; scan again before queuing it.",
                        c.name
                    ),
                ));
                continue;
            }
            let mut expected = Expected::from_facts(&checked.facts);
            if expected.is_dir {
                expected.size = c.logical;
            }
            if expected.file_ref.is_synthetic() {
                result.refused.push(refuse(
                    RefusalReason::Unverifiable,
                    format!(
                        "Windows reports no stable file id for {}, so it can't be deleted safely.",
                        c.name
                    ),
                ));
                continue;
            }
            let mut g = lock(&self.0);
            g.next_id += 1;
            let id = g.next_id;
            let entry = QueueEntry {
                id,
                volume_id: c.volume_id.clone(),
                entry_id: c.entry_id,
                path: c.path.clone(),
                name: c.name.clone(),
                is_dir: c.is_dir,
                bytes: c.bytes,
                safety: c.safety,
                category: c.category,
                rule_id: c.rule_id.clone(),
                rule_name: c.rule_name.clone(),
                explain: c.explain.clone(),
                regenerable: c.regenerable,
                app: c.app.clone(),
                added_ms: unix_ms(),
                source,
            };
            g.items.push(Queued {
                entry: entry.clone(),
                item: QueueItem {
                    id,
                    path: PathBuf::from(&c.path),
                    expected,
                    safety: c.safety,
                },
            });
            result.added.push(entry);
        }
        result
    }

    /// Removes queue ids.
    pub fn remove(&self, ids: &BTreeSet<u64>) {
        lock(&self.0).items.retain(|q| !ids.contains(&q.entry.id));
    }

    /// Empties the queue.
    pub fn clear(&self) {
        lock(&self.0).items.clear();
    }

    fn items(&self, ids: &[u64]) -> (Vec<QueueItem>, Vec<QueueEntry>) {
        let g = lock(&self.0);
        let wanted: BTreeSet<u64> = ids.iter().copied().collect();
        g.items
            .iter()
            .filter(|q| wanted.contains(&q.entry.id))
            .map(|q| (q.item.clone(), q.entry.clone()))
            .unzip()
    }

    fn remember_plan(&self, plan_id: u64, entries: Vec<QueueEntry>) {
        let mut g = lock(&self.0);
        if g.plans.len() > 64 {
            let oldest = g.plans.keys().min().copied();
            if let Some(k) = oldest {
                g.plans.remove(&k);
            }
        }
        g.plans.insert(plan_id, entries);
    }

    fn plan_entries(&self, plan_id: u64) -> Vec<QueueEntry> {
        lock(&self.0)
            .plans
            .get(&plan_id)
            .cloned()
            .unwrap_or_default()
    }
}

fn queue<R: Runtime>(app: &AppHandle<R>) -> FeatureResult<tauri::State<'_, CleanupQueue>> {
    app.try_state::<CleanupQueue>()
        .ok_or_else(|| FeatureError::internal("the cleanup queue is not initialized"))
}

fn emit_queue<R: Runtime>(app: &AppHandle<R>) {
    if let Ok(q) = queue(app) {
        let _ = app.emit(QUEUE_CHANGED, q.list());
    }
}

fn service<R: Runtime>(app: &AppHandle<R>) -> FeatureResult<Arc<CleanupService>> {
    super::cleanup::service(app)
}

fn app_state<R: Runtime>(app: &AppHandle<R>) -> FeatureResult<Arc<AppState>> {
    app.try_state::<Arc<AppState>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| FeatureError::internal("app state missing"))
}

/// Adds index entries to the queue (shared by the queue, insights,
/// recommendations and duplicates commands) and emits the queue.
///
/// # Errors
///
/// When the safety guard cannot be built.
pub fn add_to_queue<R: Runtime>(
    app: &AppHandle<R>,
    candidates: Vec<Candidate>,
    mut refused: Vec<QueueRefusal>,
    source: QueueSource,
) -> FeatureResult<QueueAddResult> {
    let guard = super::cleanup::machine_guard(&protected_dirs(app))?;
    let q = queue(app)?;
    let mut r = q.add(&guard, candidates, source);
    refused.append(&mut r.refused);
    r.refused = refused;
    if !r.added.is_empty() {
        emit_queue(app);
    }
    Ok(r)
}

fn protected_dirs<R: Runtime>(app: &AppHandle<R>) -> Vec<PathBuf> {
    app.path().app_local_data_dir().ok().into_iter().collect()
}

/// The queue (`cleanup_queue_list`).
#[tauri::command]
pub fn cleanup_queue_list<R: Runtime>(app: AppHandle<R>) -> FeatureResult<Vec<QueueEntry>> {
    Ok(queue(&app)?.list())
}

/// Adds index entries (`cleanup_queue_add`).
#[tauri::command]
pub async fn cleanup_queue_add<R: Runtime>(
    app: AppHandle<R>,
    volume_id: String,
    ids: Vec<u32>,
) -> FeatureResult<QueueAddResult> {
    let st = app_state(&app)?;
    blocking(move || {
        let (c, refused) = candidates(&st, &volume_id, &ids);
        add_to_queue(&app, c, refused, QueueSource::Manual)
    })
    .await
}

/// Removes queue items (`cleanup_queue_remove`).
#[tauri::command]
pub fn cleanup_queue_remove<R: Runtime>(app: AppHandle<R>, ids: Vec<u64>) -> FeatureResult<()> {
    queue(&app)?.remove(&ids.into_iter().collect());
    emit_queue(&app);
    Ok(())
}

/// Empties the queue (`cleanup_queue_clear`).
#[tauri::command]
pub fn cleanup_queue_clear<R: Runtime>(app: AppHandle<R>) -> FeatureResult<()> {
    queue(&app)?.clear();
    emit_queue(&app);
    Ok(())
}

// -----------------------------------------------------------------------------
// Plan
// -----------------------------------------------------------------------------

/// Items and bytes per tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TierTotal {
    /// Tier.
    pub safety: Safety,
    /// Items.
    pub items: u64,
    /// Allocated bytes.
    pub bytes: u64,
}

/// Items on one volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanVolume {
    /// Mount point.
    pub mount_point: String,
    /// Recycle Bin support (engine shape, `state` tag).
    pub recycle_bin: RecycleBinSupport,
    /// Items.
    pub items: u64,
    /// Bytes.
    pub bytes: u64,
}

/// A review warning (`PlanWarning` in the UI; refusals flattened).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanWarningDto {
    /// Removed by the never-list.
    Refused {
        /// Queue id.
        id: u64,
        /// Why.
        message: String,
    },
    /// Queued twice.
    Duplicate {
        /// Queue id.
        id: u64,
    },
    /// Inside another queued folder.
    Nested {
        /// Queue id.
        id: u64,
        /// The containing queue id.
        inside: u64,
    },
    /// Never tier.
    NeverTier {
        /// Queue id.
        id: u64,
    },
    /// Careful tier: needs a tick.
    NeedsAcknowledgement {
        /// Queue id.
        id: u64,
    },
    /// Cannot be recycled.
    CannotRecycle {
        /// Queue id.
        id: u64,
        /// Fit.
        fit: RecycleFit,
    },
    /// An owning app is running.
    RunningApp {
        /// Queue id.
        id: u64,
        /// The warning.
        warning: strata_clean::apps::RunningAppWarning,
    },
}

impl From<&PlanWarning> for PlanWarningDto {
    fn from(w: &PlanWarning) -> Self {
        match w {
            PlanWarning::Refused { id, refusal } => Self::Refused {
                id: *id,
                message: refusal.message(),
            },
            PlanWarning::Duplicate { id } => Self::Duplicate { id: *id },
            PlanWarning::Nested { id, inside } => Self::Nested {
                id: *id,
                inside: *inside,
            },
            PlanWarning::NeverTier { id } => Self::NeverTier { id: *id },
            PlanWarning::NeedsAcknowledgement { id } => Self::NeedsAcknowledgement { id: *id },
            PlanWarning::CannotRecycle { id, fit } => Self::CannotRecycle { id: *id, fit: *fit },
            PlanWarning::RunningApp { id, warning } => Self::RunningApp {
                id: *id,
                warning: warning.clone(),
            },
        }
    }
}

/// The reviewed plan (`CleanupPlan` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupPlan {
    /// Handle for pre-flight, execute and retry.
    pub plan_id: u64,
    /// Entries pre-flight and execute consider.
    pub items: Vec<QueueEntry>,
    /// Totals per tier over `items`.
    pub totals: Vec<TierTotal>,
    /// Volumes with their Recycle Bin support.
    pub volumes: Vec<PlanVolume>,
    /// Review warnings.
    pub warnings: Vec<PlanWarningDto>,
    /// Permanent deletes above this need a second confirmation.
    pub large_delete_bytes: u64,
    /// Method preselected from settings.
    pub default_method: DeleteMethod,
}

fn plan_dto(store: &Store, plan_id: u64, plan: &Plan, entries: &[QueueEntry]) -> CleanupPlan {
    let by_id: HashMap<u64, &QueueEntry> = entries.iter().map(|e| (e.id, e)).collect();
    let items: Vec<QueueEntry> = plan
        .items
        .iter()
        .filter_map(|i| by_id.get(&i.id).map(|e| (*e).clone()))
        .collect();
    let totals = [
        Safety::Safe,
        Safety::Probably,
        Safety::Careful,
        Safety::Never,
    ]
    .into_iter()
    .map(|s| {
        let of = items.iter().filter(|i| i.safety == s);
        TierTotal {
            safety: s,
            items: of.clone().count() as u64,
            bytes: of.map(|i| i.bytes).sum(),
        }
    })
    .collect();
    let settings = store.load_settings().unwrap_or_default();
    CleanupPlan {
        plan_id,
        items,
        totals,
        volumes: plan
            .volumes
            .iter()
            .map(|v| PlanVolume {
                mount_point: v.mount_point.clone(),
                recycle_bin: v.recycle_bin.clone(),
                items: v.items,
                bytes: v.bytes,
            })
            .collect(),
        warnings: plan.warnings.iter().map(Into::into).collect(),
        large_delete_bytes: settings.cleanup.large_delete_confirm_bytes,
        default_method: match settings.cleanup.default_method {
            strata_store::CleanupMethod::Permanent => DeleteMethod::Permanent,
            strata_store::CleanupMethod::RecycleBin => DeleteMethod::RecycleBin,
        },
    }
}

/// Builds the review plan for queue ids (`cleanup_plan`).
#[tauri::command]
pub async fn cleanup_plan<R: Runtime>(
    app: AppHandle<R>,
    queue_ids: Vec<u64>,
) -> FeatureResult<CleanupPlan> {
    let s = service(&app)?;
    let store = super::store::handle(&app)?;
    blocking(move || {
        let q = queue(&app)?;
        let (items, entries) = q.items(&queue_ids);
        let planned = s.plan(items)?;
        q.remember_plan(planned.plan_id, entries.clone());
        Ok(plan_dto(&store, planned.plan_id, &planned.plan, &entries))
    })
    .await
}

// -----------------------------------------------------------------------------
// Pre-flight
// -----------------------------------------------------------------------------

/// The confirmations of a decision (UI shape).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcksDto {
    /// Careful-tier queue ids the user ticked.
    pub careful: Vec<u64>,
    /// Permanent deletion confirmed.
    pub permanent: bool,
    /// Second confirmation above the threshold.
    pub large_permanent: bool,
    /// Items the bin can't take, deleted permanently instead.
    pub permanent_instead_of_recycle: Vec<u64>,
}

/// Method, confirmations and skipped queue ids (`Decision` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionDto {
    /// `recycle_bin` or `permanent`.
    pub method: DeleteMethod,
    /// Confirmations.
    #[serde(default)]
    pub acks: AcksDto,
    /// Queue ids dropped before pre-flight and execute.
    #[serde(default)]
    pub skip: Vec<u64>,
}

impl DecisionDto {
    /// The engine decision and the skip set.
    #[must_use]
    pub fn split(&self) -> (Decision, BTreeSet<u64>) {
        (
            Decision {
                method: self.method,
                acks: Acknowledgements {
                    careful: self.acks.careful.iter().copied().collect(),
                    permanent: self.acks.permanent,
                    large_permanent: self.acks.large_permanent,
                    permanent_instead_of_recycle: self
                        .acks
                        .permanent_instead_of_recycle
                        .iter()
                        .copied()
                        .collect(),
                },
            },
            self.skip.iter().copied().collect(),
        )
    }
}

/// A process holding a file open (`LockHolder` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockHolderDto {
    /// Process id.
    pub pid: u32,
    /// Process start time (FILETIME).
    pub start_time: u64,
    /// Friendly name.
    pub app_name: String,
    /// Executable path.
    pub exe_path: Option<String>,
    /// Service name.
    pub service: Option<String>,
    /// Application type.
    pub kind: AppKind,
    /// Registered for restart.
    pub restartable: bool,
}

impl From<&LockHolder> for LockHolderDto {
    fn from(h: &LockHolder) -> Self {
        Self {
            pid: h.pid,
            start_time: h.start_time,
            app_name: h.app_name.clone(),
            exe_path: h.exe_path.clone(),
            service: h.service.clone(),
            kind: h.kind,
            restartable: h.restartable,
        }
    }
}

/// A cleanup failure flattened for display (`CleanErrorInfo` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanErrorInfo {
    /// The engine's serde tag.
    pub kind: String,
    /// `CleanError::message()`.
    pub message: String,
    /// `CleanError::is_retryable()`.
    pub retryable: bool,
    /// The item path, when the error names one.
    pub path: Option<String>,
    /// Lock holders for `locked`.
    pub holders: Vec<LockHolderDto>,
}

impl From<&CleanError> for CleanErrorInfo {
    fn from(e: &CleanError) -> Self {
        let v = serde_json::to_value(e).unwrap_or_default();
        Self {
            kind: v["kind"].as_str().unwrap_or("os").to_owned(),
            message: e.message(),
            retryable: e.is_retryable(),
            path: v["path"].as_str().map(str::to_owned),
            holders: match e {
                CleanError::Locked { holders, .. } => holders.iter().map(Into::into).collect(),
                _ => Vec::new(),
            },
        }
    }
}

/// Pre-flight verdict (`Verdict` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum VerdictDto {
    /// Ready to act.
    Ready {
        /// Recycle Bin fit.
        recycle: RecycleFit,
    },
    /// Blocked.
    Blocked {
        /// Why.
        error: CleanErrorInfo,
    },
}

/// Pre-flight result of one item (`ItemVerdict` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemVerdictDto {
    /// Queue id.
    pub id: u64,
    /// Path.
    pub path: String,
    /// Verdict.
    pub verdict: VerdictDto,
    /// Lock holders.
    pub holders: Vec<LockHolderDto>,
    /// Running-app warnings.
    pub running_apps: Vec<strata_clean::apps::RunningAppWarning>,
}

impl From<&ItemVerdict> for ItemVerdictDto {
    fn from(v: &ItemVerdict) -> Self {
        Self {
            id: v.id,
            path: v.path.clone(),
            verdict: match &v.verdict {
                Verdict::Ready { recycle } => VerdictDto::Ready { recycle: *recycle },
                Verdict::Blocked { error } => VerdictDto::Blocked {
                    error: error.into(),
                },
            },
            holders: v.holders.iter().map(Into::into).collect(),
            running_apps: v.running_apps.clone(),
        }
    }
}

/// Re-verifies every non-skipped item right before acting
/// (`cleanup_preflight`).
#[tauri::command]
pub async fn cleanup_preflight<R: Runtime>(
    app: AppHandle<R>,
    plan_id: u64,
    decision: DecisionDto,
) -> FeatureResult<Vec<ItemVerdictDto>> {
    let s = service(&app)?;
    let store = super::store::handle(&app)?;
    blocking(move || {
        let cfg = config_from(&store)?;
        let (d, skip) = decision.split();
        let verdicts = s.preflight(plan_id, &d.acks, &cfg, &skip)?;
        if let Some(h) = app.try_state::<KnownHolders>() {
            h.remember(verdicts.iter().flat_map(|v| v.holders.iter().cloned()));
        }
        Ok(verdicts.iter().map(Into::into).collect())
    })
    .await
}

// -----------------------------------------------------------------------------
// Polite close
// -----------------------------------------------------------------------------

/// The consent prompt for closing a lock holder (`ClosePrompt` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosePrompt {
    /// Handle for `cleanup_close_app`.
    pub prompt_id: u64,
    /// Text shown verbatim.
    pub message: String,
    /// App name.
    pub app: String,
    /// Process id.
    pub pid: u32,
    /// Unix ms after which the prompt is void.
    pub expires_ms: i64,
}

/// Builds the "ask X to close" prompt for a holder a lock check or pre-flight
/// reported (`cleanup_close_prompt`). Closes nothing.
#[tauri::command]
pub fn cleanup_close_prompt<R: Runtime>(
    app: AppHandle<R>,
    pid: u32,
    start_time: u64,
) -> FeatureResult<ClosePrompt> {
    let holder = app
        .try_state::<KnownHolders>()
        .and_then(|k| k.get(pid, start_time))
        .ok_or_else(|| {
            FeatureError::new(
                ErrorKind::NotFound,
                "that program is not in a recent lock check; run the pre-flight again",
            )
        })?;
    let prompt = strata_clean::consent::Prompt::new(holder.close_request());
    let message = prompt.text();
    let app_name = holder.app_name.clone();
    let pending = app
        .try_state::<PendingCloses>()
        .ok_or_else(|| FeatureError::internal("lock state missing"))?;
    let (prompt_id, expires_ms) = pending
        .0
        .offer_numbered(PendingClose { holder, prompt }, Instant::now())?;
    Ok(ClosePrompt {
        prompt_id,
        message,
        app: app_name,
        pid,
        expires_ms,
    })
}

/// Closes the app politely after the user confirmed the prompt
/// (`cleanup_close_app`). Never a silent kill.
#[tauri::command]
pub async fn cleanup_close_app<R: Runtime>(
    app: AppHandle<R>,
    prompt_id: u64,
) -> FeatureResult<CloseOutcome> {
    let pending = app
        .try_state::<PendingCloses>()
        .ok_or_else(|| FeatureError::internal("lock state missing"))?;
    let PendingClose { holder, prompt } = pending.0.take_numbered(prompt_id, Instant::now())?;
    blocking(move || {
        // This handler is the user's confirmation click; the consent is
        // minted and redeemed on this thread.
        strata_clean::locks::close_politely(&holder, prompt.confirm())
            .map_err(|e| FeatureError::with_detail(ErrorKind::CloseApp, e.to_string(), &e))
    })
    .await
}

// -----------------------------------------------------------------------------
// Execute
// -----------------------------------------------------------------------------

/// What happened to one item (`ItemOutcome` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutcomeDto {
    /// In the Recycle Bin; restorable through `cleanup_restore`.
    Recycled {
        /// Undo-log item id.
        #[serde(rename = "restoreItemId")]
        restore_item_id: Option<i64>,
    },
    /// Deleted permanently.
    Deleted {
        /// Bytes removed.
        bytes: u64,
    },
    /// Failed.
    Failed {
        /// Why.
        error: CleanErrorInfo,
    },
    /// Skipped.
    Skipped {
        /// Why.
        reason: CleanErrorInfo,
    },
}

/// One item's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemResultDto {
    /// Queue id.
    pub id: u64,
    /// Path.
    pub path: String,
    /// Outcome.
    pub outcome: OutcomeDto,
}

/// Result of an execution (`ExecutionReport` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionReportDto {
    /// Undo-log action id.
    pub action_id: Option<i64>,
    /// Per-item results.
    pub results: Vec<ItemResultDto>,
    /// Totals.
    pub summary: strata_clean::audit::ActionSummary,
}

fn report_dto(store: &Store, report: &ExecutionReport) -> ExecutionReportDto {
    let action_id = report.action.and_then(|a| i64::try_from(a.0).ok());
    // Undo-log items are written in plan order with the queued path.
    let logged: HashMap<String, i64> = action_id
        .and_then(|a| store.action(ActionId(a)).ok())
        .map(|r| {
            r.items
                .iter()
                .map(|i| (path_key(&i.planned.path), i.id.0))
                .collect()
        })
        .unwrap_or_default();
    ExecutionReportDto {
        action_id,
        results: report
            .results
            .iter()
            .map(|r| ItemResultDto {
                id: r.id,
                path: r.path.clone(),
                outcome: match &r.outcome {
                    ItemOutcome::Recycled { .. } => OutcomeDto::Recycled {
                        restore_item_id: logged.get(&path_key(&r.path)).copied(),
                    },
                    ItemOutcome::Deleted { stats } => OutcomeDto::Deleted { bytes: stats.bytes },
                    ItemOutcome::Failed { error } => OutcomeDto::Failed {
                        error: error.into(),
                    },
                    ItemOutcome::Skipped { reason } => OutcomeDto::Skipped {
                        reason: reason.into(),
                    },
                },
            })
            .collect(),
        summary: report.summary,
    }
}

/// Drops removed items from the queue and from the indexes, so every view
/// shows the space as freed without a rescan.
fn after_run<R: Runtime>(app: &AppHandle<R>, plan_id: u64, report: &ExecutionReport) {
    let removed: BTreeSet<u64> = report
        .results
        .iter()
        .filter(|r| r.outcome.removed())
        .map(|r| r.id)
        .collect();
    if removed.is_empty() {
        return;
    }
    if let Ok(q) = queue(app) {
        let entries = q.plan_entries(plan_id);
        q.remove(&removed);
        emit_queue(app);
        if let Ok(st) = app_state(app) {
            let gone: Vec<(String, u32)> = entries
                .iter()
                .filter(|e| removed.contains(&e.id))
                .map(|e| (e.volume_id.clone(), e.entry_id))
                .collect();
            crate::jobs::forget_entries(app, &st, &gone);
        }
    }
}

/// Executes the plan (`cleanup_execute`), streaming progress. Never falls
/// back from recycling to a permanent delete on its own.
#[tauri::command]
pub async fn cleanup_execute<R: Runtime>(
    app: AppHandle<R>,
    plan_id: u64,
    decision: DecisionDto,
    on_progress: Channel<Progress>,
) -> FeatureResult<ExecutionReportDto> {
    let s = service(&app)?;
    let store = super::store::handle(&app)?;
    blocking(move || {
        let cfg = config_from(&store)?;
        let (d, skip) = decision.split();
        let report = s.execute(plan_id, &d, &cfg, &store, &skip, &mut |p| {
            let _ = on_progress.send(p);
        })?;
        after_run(&app, plan_id, &report);
        Ok(report_dto(&store, &report))
    })
    .await
}

/// Executes the plan through the elevated helper: permanent deletes by file
/// id (`cleanup_execute_elevated`).
#[tauri::command]
pub async fn cleanup_execute_elevated<R: Runtime>(
    app: AppHandle<R>,
    plan_id: u64,
    decision: DecisionDto,
    on_progress: Channel<Progress>,
) -> FeatureResult<ExecutionReportDto> {
    let s = service(&app)?;
    let store = super::store::handle(&app)?;
    blocking(move || {
        let cfg = config_from(&store)?;
        let (d, skip) = decision.split();
        let report = s.execute_elevated(plan_id, &d, &cfg, &store, &skip, &mut |p| {
            let _ = on_progress.send(p);
        })?;
        after_run(&app, plan_id, &report);
        Ok(report_dto(&store, &report))
    })
    .await
}

/// Cancels a running execution (`cleanup_cancel`).
#[tauri::command]
pub fn cleanup_cancel<R: Runtime>(app: AppHandle<R>, plan_id: u64) -> FeatureResult<()> {
    service(&app)?.cancel(plan_id);
    Ok(())
}

/// A plan of the retryable failures of a finished run
/// (`cleanup_retry_plan`); pre-flight runs again before it acts.
#[tauri::command]
pub async fn cleanup_retry_plan<R: Runtime>(
    app: AppHandle<R>,
    plan_id: u64,
) -> FeatureResult<CleanupPlan> {
    let s = service(&app)?;
    let store = super::store::handle(&app)?;
    blocking(move || {
        let q = queue(&app)?;
        let entries = q.plan_entries(plan_id);
        let planned = s.retry(plan_id)?;
        q.remember_plan(planned.plan_id, entries.clone());
        Ok(plan_dto(&store, planned.plan_id, &planned.plan, &entries))
    })
    .await
}

/// Schedules a locked plain file for deletion at the next restart through
/// the helper (`cleanup_delete_on_reboot`), and logs it.
#[tauri::command]
pub async fn cleanup_delete_on_reboot<R: Runtime>(
    app: AppHandle<R>,
    plan_id: u64,
    id: u64,
) -> FeatureResult<()> {
    let s = service(&app)?;
    let store = super::store::handle(&app)?;
    blocking(move || {
        let helper = s.privileged().ok_or_else(|| {
            FeatureError::unavailable("deleting at restart needs the elevated helper (fast scan)")
        })?;
        let plan = s
            .plan_of(plan_id)
            .ok_or_else(|| FeatureError::new(ErrorKind::UnknownPlan, "that plan has expired"))?;
        let item = plan
            .items
            .iter()
            .find(|i| i.id == id)
            .ok_or_else(|| FeatureError::not_found("that item is not in the plan"))?;
        let req = super::cleanup::privileged_request(item)?;
        let planned = strata_store::PlannedItem {
            path: item.path.display().to_string(),
            volume: strata_store::VolumeKey {
                serial: 0,
                guid_path: req.volume.clone(),
            },
            file_ref: item.expected.file_ref,
            size: item.expected.size,
            mtime: item.expected.modified,
            method: strata_store::DeleteMethod::RebootDelete,
            tier: item.safety,
            rule_id: None,
        };
        let action = store.begin_action(ActionKind::Cleanup, std::slice::from_ref(&planned))?;
        let result = helper.delete_on_reboot(&req);
        let outcome = strata_store::ItemOutcome {
            result: if result.is_ok() {
                strata_store::ItemResult::Done
            } else {
                strata_store::ItemResult::Failed
            },
            error: result.as_ref().err().map(CleanError::message),
            restore: None,
        };
        let _ = store.complete_item(action, 0, &outcome);
        let _ = store.finish_action(
            action,
            if result.is_ok() {
                ActionStatus::Completed
            } else {
                ActionStatus::Failed
            },
        );
        result.map_err(Into::into)
    })
    .await
}

// -----------------------------------------------------------------------------
// Undo history and restore
// -----------------------------------------------------------------------------

/// One item of a logged action (`UndoItem` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoItem {
    /// Item id.
    pub item_id: i64,
    /// Path at the time.
    pub path: String,
    /// Bytes.
    pub bytes: u64,
    /// `recycle`, `permanent`, `reboot_delete` or `tool`.
    pub method: strata_store::DeleteMethod,
    /// Tier.
    pub tier: Safety,
    /// `pending`, `done`, `failed` or `skipped`.
    pub result: strata_store::ItemResult,
    /// Failure or skip reason.
    pub error: Option<String>,
    /// Unix ms.
    pub completed_ms: Option<i64>,
    /// Restorable now.
    pub restorable: bool,
    /// Unix ms, when restored.
    pub restored_ms: Option<i64>,
}

impl From<&ItemRecord> for UndoItem {
    fn from(i: &ItemRecord) -> Self {
        Self {
            item_id: i.id.0,
            path: i.planned.path.clone(),
            bytes: i.planned.size,
            method: i.planned.method,
            tier: i.planned.tier,
            result: i.result,
            error: i.error.clone(),
            completed_ms: i.completed_at.map(|t| t.0.saturating_mul(1000)),
            restorable: is_restorable(i),
            restored_ms: i.restored_at.map(|t| t.0.saturating_mul(1000)),
        }
    }
}

/// One logged action (`UndoAction` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoAction {
    /// Action id.
    pub action_id: i64,
    /// `cleanup`, `duplicates` or `tool`.
    pub kind: ActionKind,
    /// Status.
    pub status: ActionStatus,
    /// Unix ms.
    pub started_ms: i64,
    /// Unix ms.
    pub finished_ms: Option<i64>,
    /// Items planned.
    pub item_count: u64,
    /// Items removed.
    pub done_count: u64,
    /// Items failed.
    pub failed_count: u64,
    /// Bytes removed.
    pub bytes_done: u64,
    /// Items in plan order.
    pub items: Vec<UndoItem>,
}

/// The undo history, newest first (`cleanup_history`).
#[tauri::command]
pub async fn cleanup_history<R: Runtime>(
    app: AppHandle<R>,
    limit: Option<usize>,
    before_action_id: Option<i64>,
) -> FeatureResult<Vec<UndoAction>> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        let rows = store.action_history(
            limit.unwrap_or(50).clamp(1, 500),
            before_action_id.map(ActionId),
        )?;
        Ok(rows
            .iter()
            .map(|s| UndoAction {
                action_id: s.id.0,
                kind: s.kind,
                status: s.status,
                started_ms: s.started_at.0.saturating_mul(1000),
                finished_ms: s.finished_at.map(|t| t.0.saturating_mul(1000)),
                item_count: s.item_count,
                done_count: s.done_count,
                failed_count: s.failed_count,
                bytes_done: s.bytes_done,
                items: store
                    .action(s.id)
                    .map(|r| r.items.iter().map(Into::into).collect())
                    .unwrap_or_default(),
            })
            .collect())
    })
    .await
}

/// Result of restoring one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    /// Item id.
    pub item_id: i64,
    /// Restored.
    pub ok: bool,
    /// Why not.
    pub message: Option<String>,
}

/// Restores recycled items to their original paths; never overwrites
/// (`cleanup_restore`).
#[tauri::command]
pub async fn cleanup_restore<R: Runtime>(
    app: AppHandle<R>,
    item_ids: Vec<i64>,
) -> FeatureResult<Vec<RestoreResult>> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        let restorable = store.restorable_items(10_000)?;
        Ok(item_ids
            .iter()
            .map(|&item_id| {
                let r = restorable
                    .iter()
                    .find(|i| i.id == ItemId(item_id))
                    .ok_or_else(|| {
                        FeatureError::invalid(
                            "this item is not in the Recycle Bin from Strata or was already restored",
                        )
                    })
                    .and_then(|i| restore_item(&store, i.action.0, item_id));
                RestoreResult {
                    item_id,
                    ok: r.is_ok(),
                    message: r.err().map(|e| e.message),
                }
            })
            .collect())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_deserialize_from_the_ui_shape() {
        let d: DecisionDto = serde_json::from_str(
            r#"{"method":"permanent","acks":{"careful":[3],"permanent":true,"largePermanent":false,"permanentInsteadOfRecycle":[4]},"skip":[7,8]}"#,
        )
        .unwrap();
        let (decision, skip) = d.split();
        assert_eq!(decision.method, DeleteMethod::Permanent);
        assert!(decision.acks.careful.contains(&3) && decision.acks.permanent);
        assert!(decision.acks.permanent_instead_of_recycle.contains(&4));
        assert_eq!(skip, BTreeSet::from([7, 8]));
    }

    #[test]
    fn paths_nest_by_component() {
        assert!(is_inside(&path_key(r"C:\a\b"), &path_key(r"c:\A")));
        assert!(!is_inside(&path_key(r"C:\ab"), &path_key(r"C:\a")));
        assert_eq!(path_key(r"\\?\C:\X\"), r"c:\x");
    }

    #[test]
    fn errors_flatten_with_their_tag_and_holders() {
        let e = CleanError::Locked {
            path: r"D:\x.tmp".into(),
            holders: vec![LockHolder {
                pid: 7,
                start_time: 9,
                app_name: "Example".into(),
                exe_path: None,
                service: None,
                kind: AppKind::MainWindow,
                restartable: false,
            }],
        };
        let info = CleanErrorInfo::from(&e);
        assert_eq!(info.kind, "locked");
        assert_eq!(info.path.as_deref(), Some(r"D:\x.tmp"));
        assert_eq!(info.holders[0].start_time, 9);
        let v = serde_json::to_value(&info.holders[0]).unwrap();
        assert_eq!(v["appName"], "Example");
        let outcome = serde_json::to_value(OutcomeDto::Recycled {
            restore_item_id: Some(5),
        })
        .unwrap();
        assert_eq!(outcome["kind"], "recycled");
        assert_eq!(outcome["restoreItemId"], 5);
    }
}
