//! Cleanup commands over `strata-clean`'s flow (SPEC Â§15.2-Â§15.7).
//!
//! Responsibilities:
//! - [`CleanupService`]: plans kept on the backend (the UI refers to them by
//!   id and can never hand back an edited plan), pre-flight, execution with
//!   cancellation, retry, restore, and the elevated route.
//! - The never-list enforced again at this layer, on the way in (plan) and
//!   on the way out (pre-flight and execute), independently of the flow.
//! - [`PrivilegedBackend`]: the hook the helper client satisfies so items
//!   that need elevation are deleted by the helper, by file id.
//! - Undo history and Restore over the store.
//!
//! Cleanup stays disabled ([`Readiness::Recovering`]) until crash recovery
//! of the undo log has finished at startup.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use strata_clean::audit::{AuditAction, AuditItem, AuditLog, DeleteMethod, ItemOutcome};
use strata_clean::canon::CanonicalPath;
use strata_clean::flow::{
    self, Acknowledgements, CleanupConfig, Decision, ExecutionReport, ItemResult, Plan,
    PlanWarning, Progress, QueueItem,
};
use strata_clean::permanent::DeleteStats;
use strata_clean::preflight::ItemVerdict;
use strata_clean::privileged::PrivilegedDeleteRequest;
use strata_clean::recycle::{self, RestoreTicket};
use strata_clean::tools::{CommandSpec, ToolError, ToolOutput};
use strata_clean::{CancelToken, CleanError, GuardConfig, SafetyGuard};
use strata_core::{FileTime, Safety};
use strata_store::{ActionId, ActionRecord, ActionSummary, ItemId, ItemRecord, Store, Timestamp};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, Runtime};

use super::audit::StoreAuditLog;
use super::error::{ErrorKind, FeatureError, FeatureResult, blocking};

// -----------------------------------------------------------------------------
// Privileged hook
// -----------------------------------------------------------------------------

/// What the elevated helper offers the app. The helper client (backend
/// track) implements this and installs it with [`set_privileged_backend`].
///
/// The helper re-validates everything itself (SPEC Â§15.7); this layer has
/// already checked the never-list and the user's confirmations.
pub trait PrivilegedBackend: Send + Sync {
    /// Deletes one item by file id, permanently, in the helper.
    ///
    /// # Errors
    ///
    /// The helper's typed refusal or failure.
    fn delete_by_id(&self, request: &PrivilegedDeleteRequest) -> Result<DeleteStats, CleanError>;

    /// Runs an elevated tool (DISM) in the helper with output captured.
    /// `None` means the helper cannot, and the app falls back to a UAC
    /// launch.
    fn run_tool(&self, spec: &CommandSpec) -> Option<Result<ToolOutput, ToolError>> {
        let _ = spec;
        None
    }
}

/// Installs (or with `None` removes, e.g. on helper disconnect) the helper
/// client used for privileged deletes and elevated tools.
pub fn set_privileged_backend<R: Runtime>(
    app: &AppHandle<R>,
    backend: Option<Arc<dyn PrivilegedBackend>>,
) {
    if let Some(s) = app.try_state::<CleanupState>() {
        s.service().set_privileged(backend);
    }
}

// -----------------------------------------------------------------------------
// Service
// -----------------------------------------------------------------------------

/// Pre-flight results are trusted for this long before execute needs a new
/// one (SPEC Â§15.2 step 5: "right before acting").
pub const PREFLIGHT_MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// Plans nobody touched for this long are dropped.
const PLAN_MAX_AGE: Duration = Duration::from_secs(4 * 60 * 60);

/// Whether cleanup may act.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Readiness {
    /// Crash recovery of the undo log is still running.
    Recovering,
    /// Ready.
    Ready,
    /// Cannot act (for example `state.db` is damaged).
    Unavailable {
        /// Why.
        reason: String,
    },
}

/// Builds a fresh [`SafetyGuard`] (volumes and known folders can change
/// while the app runs, so each plan and pre-flight gets a new one).
pub type GuardFactory = Box<dyn Fn() -> FeatureResult<SafetyGuard> + Send + Sync>;

struct PlanEntry {
    plan: Plan,
    touched: Instant,
    preflight: Option<(Instant, Arc<SafetyGuard>)>,
    cancel: Option<CancelToken>,
    report: Option<ExecutionReport>,
}

/// A plan as returned to the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanResponse {
    /// Id for pre-flight, execute, retry and cancel.
    pub plan_id: u64,
    /// The reviewed plan (`strata_clean::flow::Plan`, snake_case fields).
    pub plan: Plan,
}

/// The cleanup engine of the app, independent of Tauri so it can be tested.
pub struct CleanupService {
    readiness: RwLock<Readiness>,
    plans: Mutex<HashMap<u64, PlanEntry>>,
    next_id: AtomicU64,
    running: Mutex<bool>,
    guards: GuardFactory,
    privileged: RwLock<Option<Arc<dyn PrivilegedBackend>>>,
}

impl std::fmt::Debug for CleanupService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CleanupService")
            .field("readiness", &self.readiness())
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Drops never-list hits before they reach the flow. Returns the kept items
/// and a `Refused` warning per dropped item.
#[must_use]
pub fn enforce_never_list(
    guard: &SafetyGuard,
    items: Vec<QueueItem>,
) -> (Vec<QueueItem>, Vec<PlanWarning>) {
    let mut kept = Vec::with_capacity(items.len());
    let mut refused = Vec::new();
    for item in items {
        match guard.never_list().check_str(item.path.as_os_str()) {
            Ok(_) => kept.push(item),
            Err(refusal) => refused.push(PlanWarning::Refused {
                id: item.id,
                refusal,
            }),
        }
    }
    (kept, refused)
}

fn check_plan_against(guard: &SafetyGuard, plan: &Plan) -> FeatureResult<()> {
    for item in &plan.items {
        if let Err(refusal) = guard.never_list().check_str(item.path.as_os_str()) {
            return Err(FeatureError::with_detail(
                ErrorKind::Refused,
                refusal.message(),
                &refusal,
            ));
        }
    }
    Ok(())
}

/// The confirmations the elevated route requires; mirrors the flow's gate
/// with the method fixed to permanent (the helper never recycles).
///
/// # Errors
///
/// The [`CleanError`] the flow would report for the same item.
pub fn elevated_gate(
    item: &QueueItem,
    acks: &Acknowledgements,
    cfg: &CleanupConfig,
) -> Result<(), CleanError> {
    let path = item.path.display().to_string();
    match item.safety {
        Safety::Never => return Err(CleanError::NeverTier { path }),
        Safety::Careful if !acks.careful.contains(&item.id) => {
            return Err(CleanError::NeedsAcknowledgement {
                path,
                tier: Safety::Careful,
            });
        }
        _ => {}
    }
    if !acks.permanent {
        return Err(CleanError::NeedsPermanentConfirmation { path });
    }
    if item.expected.size > cfg.large_permanent_bytes && !acks.large_permanent {
        return Err(CleanError::NeedsLargeDeleteConfirmation {
            path,
            size: item.expected.size,
        });
    }
    Ok(())
}

fn audit_item(i: &QueueItem) -> AuditItem {
    AuditItem {
        item_id: i.id,
        path: i.path.display().to_string(),
        file_ref: i.expected.file_ref,
        size: i.expected.size,
        safety: i.safety,
    }
}

fn now_filetime() -> FileTime {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    FileTime::from_unix_secs(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// The request the helper receives for `item`.
///
/// # Errors
///
/// When the item's path cannot be parsed.
pub fn privileged_request(item: &QueueItem) -> Result<PrivilegedDeleteRequest, CleanError> {
    let canon = CanonicalPath::parse(&item.path).map_err(|_| CleanError::NotFound {
        path: item.path.display().to_string(),
    })?;
    let volume = strata_clean::volume::volume_info(&canon)
        .ok()
        .and_then(|v| v.guid.or(Some(v.mount_point)))
        .unwrap_or_else(|| {
            let mut root = canon.clone();
            while let Some(p) = root.parent() {
                root = p;
            }
            root.to_string()
        });
    Ok(PrivilegedDeleteRequest {
        volume,
        file_ref: item.expected.file_ref,
        expected_path: item.path.display().to_string(),
        expected_size: item.expected.size,
        expected_mtime: item.expected.modified,
        is_dir: item.expected.is_dir,
    })
}

struct RunGuard<'a>(&'a Mutex<bool>);

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        *lock(self.0) = false;
    }
}

impl CleanupService {
    /// A service whose guards come from `guards`. Starts in
    /// [`Readiness::Recovering`].
    #[must_use]
    pub fn new(guards: GuardFactory) -> Self {
        Self {
            readiness: RwLock::new(Readiness::Recovering),
            plans: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            running: Mutex::new(false),
            guards,
            privileged: RwLock::new(None),
        }
    }

    /// Current readiness.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        self.readiness
            .read()
            .map_or(Readiness::Recovering, |r| r.clone())
    }

    /// Enables cleanup (after recovery).
    pub fn set_ready(&self) {
        if let Ok(mut r) = self.readiness.write() {
            *r = Readiness::Ready;
        }
    }

    /// Disables cleanup with a reason.
    pub fn set_unavailable(&self, reason: String) {
        if let Ok(mut r) = self.readiness.write() {
            *r = Readiness::Unavailable { reason };
        }
    }

    /// Installs or removes the helper client.
    pub fn set_privileged(&self, backend: Option<Arc<dyn PrivilegedBackend>>) {
        if let Ok(mut p) = self.privileged.write() {
            *p = backend;
        }
    }

    /// The helper client, when connected.
    #[must_use]
    pub fn privileged(&self) -> Option<Arc<dyn PrivilegedBackend>> {
        self.privileged.read().ok().and_then(|p| p.clone())
    }

    fn require_ready(&self) -> FeatureResult<()> {
        match self.readiness() {
            Readiness::Ready => Ok(()),
            Readiness::Recovering => Err(FeatureError::new(
                ErrorKind::NotReady,
                "Strata is still checking the last cleanup; try again in a moment",
            )),
            Readiness::Unavailable { reason } => Err(FeatureError::new(
                ErrorKind::NotReady,
                format!("Cleanup is unavailable: {reason}"),
            )),
        }
    }

    fn insert(&self, plan: Plan) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut plans = lock(&self.plans);
        plans.retain(|_, e| {
            e.cancel.is_some() || now.saturating_duration_since(e.touched) < PLAN_MAX_AGE
        });
        plans.insert(
            id,
            PlanEntry {
                plan,
                touched: now,
                preflight: None,
                cancel: None,
                report: None,
            },
        );
        id
    }

    fn unknown(plan_id: u64) -> FeatureError {
        FeatureError::new(
            ErrorKind::UnknownPlan,
            format!("cleanup plan {plan_id} is unknown or expired; review the queue again"),
        )
    }

    /// Builds and stores the review plan for `items`.
    ///
    /// # Errors
    ///
    /// When the guard cannot be built (volumes or known folders unreadable).
    pub fn plan(&self, items: Vec<QueueItem>) -> FeatureResult<PlanResponse> {
        let guard = (self.guards)()?;
        let (kept, mut refused) = enforce_never_list(&guard, items);
        let mut plan = flow::plan(&guard, kept);
        refused.append(&mut plan.warnings);
        plan.warnings = refused;
        let plan_id = self.insert(plan.clone());
        Ok(PlanResponse { plan_id, plan })
    }

    /// Runs pre-flight for a stored plan and remembers it for execute.
    ///
    /// # Errors
    ///
    /// Unknown plan, guard construction, or an app-layer never-list hit.
    pub fn preflight(
        &self,
        plan_id: u64,
        acks: &Acknowledgements,
        cfg: &CleanupConfig,
    ) -> FeatureResult<Vec<ItemVerdict>> {
        let plan = lock(&self.plans)
            .get(&plan_id)
            .map(|e| e.plan.clone())
            .ok_or_else(|| Self::unknown(plan_id))?;
        let guard = Arc::new((self.guards)()?);
        check_plan_against(&guard, &plan)?;
        let verdicts = flow::preflight(&guard, &plan, acks, cfg);
        if let Some(e) = lock(&self.plans).get_mut(&plan_id) {
            e.touched = Instant::now();
            e.preflight = Some((Instant::now(), guard));
        }
        Ok(verdicts)
    }

    fn begin_run(&self, plan_id: u64) -> FeatureResult<(Plan, Arc<SafetyGuard>, CancelToken)> {
        let mut plans = lock(&self.plans);
        let entry = plans
            .get_mut(&plan_id)
            .ok_or_else(|| Self::unknown(plan_id))?;
        let guard = match &entry.preflight {
            Some((at, g)) if at.elapsed() <= PREFLIGHT_MAX_AGE => g.clone(),
            _ => {
                return Err(FeatureError::new(
                    ErrorKind::PreflightRequired,
                    "run the pre-flight check right before cleaning",
                ));
            }
        };
        let cancel = CancelToken::new();
        entry.cancel = Some(cancel.clone());
        entry.touched = Instant::now();
        Ok((entry.plan.clone(), guard, cancel))
    }

    fn end_run(&self, plan_id: u64, report: &ExecutionReport) {
        if let Some(e) = lock(&self.plans).get_mut(&plan_id) {
            e.cancel = None;
            // Everything that acted needs a fresh pre-flight before another run.
            e.preflight = None;
            e.report = Some(report.clone());
            e.touched = Instant::now();
        }
    }

    fn claim_runner(&self) -> FeatureResult<RunGuard<'_>> {
        let mut running = lock(&self.running);
        if *running {
            return Err(FeatureError::new(
                ErrorKind::Busy,
                "another cleanup is running; wait for it to finish",
            ));
        }
        *running = true;
        Ok(RunGuard(&self.running))
    }

    /// Executes a pre-flighted plan, writing the undo log to `store`.
    ///
    /// # Errors
    ///
    /// Not ready, unknown plan, no recent pre-flight, another run in
    /// progress, or an app-layer never-list hit. Per-item failures are in the
    /// report, not errors.
    pub fn execute(
        &self,
        plan_id: u64,
        decision: &Decision,
        cfg: &CleanupConfig,
        store: &Store,
        progress: &mut dyn FnMut(Progress),
    ) -> FeatureResult<ExecutionReport> {
        self.require_ready()?;
        let _run = self.claim_runner()?;
        let (plan, guard, cancel) = self.begin_run(plan_id)?;
        let result = check_plan_against(&guard, &plan).map(|()| {
            if plan.items.is_empty() {
                return ExecutionReport {
                    action: None,
                    results: Vec::new(),
                    summary: strata_clean::audit::ActionSummary::default(),
                };
            }
            let mut log = StoreAuditLog::for_plan(store.clone(), &plan, decision);
            flow::execute(&guard, &plan, decision, cfg, &mut log, progress, &cancel)
        });
        let report = match result {
            Ok(r) => r,
            Err(e) => {
                if let Some(entry) = lock(&self.plans).get_mut(&plan_id) {
                    entry.cancel = None;
                }
                return Err(e);
            }
        };
        self.end_run(plan_id, &report);
        Ok(report)
    }

    /// Executes a pre-flighted plan through the elevated helper: permanent
    /// deletes by file id, with the same write-ahead log.
    ///
    /// # Errors
    ///
    /// As [`execute`](Self::execute), plus [`ErrorKind::Unsupported`] when no
    /// helper is connected or the decision is not a confirmed permanent
    /// delete.
    pub fn execute_elevated(
        &self,
        plan_id: u64,
        decision: &Decision,
        cfg: &CleanupConfig,
        store: &Store,
        progress: &mut dyn FnMut(Progress),
    ) -> FeatureResult<ExecutionReport> {
        self.require_ready()?;
        let helper = self.privileged().ok_or_else(|| {
            FeatureError::new(
                ErrorKind::Unsupported,
                "the elevated helper is not running; enable fast scan first",
            )
        })?;
        if decision.method != DeleteMethod::Permanent || !decision.acks.permanent {
            return Err(FeatureError::invalid(
                "the helper only deletes permanently; confirm permanent deletion first",
            ));
        }
        let _run = self.claim_runner()?;
        let (plan, guard, cancel) = self.begin_run(plan_id)?;
        if let Err(e) = check_plan_against(&guard, &plan) {
            if let Some(entry) = lock(&self.plans).get_mut(&plan_id) {
                entry.cancel = None;
            }
            return Err(e);
        }
        let report = run_elevated(
            &*helper, &guard, &plan, decision, cfg, store, progress, &cancel,
        );
        self.end_run(plan_id, &report);
        Ok(report)
    }

    /// Requests cancellation of a running plan. Items already running finish.
    pub fn cancel(&self, plan_id: u64) {
        if let Some(c) = lock(&self.plans)
            .get(&plan_id)
            .and_then(|e| e.cancel.clone())
        {
            c.cancel();
        }
    }

    /// A new plan of the items in the last run worth retrying.
    ///
    /// # Errors
    ///
    /// Unknown plan, or the plan has not run yet.
    pub fn retry(&self, plan_id: u64) -> FeatureResult<PlanResponse> {
        let (plan, report) = {
            let plans = lock(&self.plans);
            let e = plans.get(&plan_id).ok_or_else(|| Self::unknown(plan_id))?;
            let report = e
                .report
                .clone()
                .ok_or_else(|| FeatureError::invalid("this plan has not run yet"))?;
            (e.plan.clone(), report)
        };
        let retry = plan.retry(&report);
        let guard = (self.guards)()?;
        let (kept, refused) = enforce_never_list(&guard, retry.items.clone());
        let retry = Plan {
            items: kept,
            warnings: refused,
            ..retry
        };
        let plan_id = self.insert(retry.clone());
        Ok(PlanResponse {
            plan_id,
            plan: retry,
        })
    }

    /// Forgets a plan.
    pub fn discard(&self, plan_id: u64) {
        let mut plans = lock(&self.plans);
        if plans.get(&plan_id).is_some_and(|e| e.cancel.is_none()) {
            plans.remove(&plan_id);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_elevated(
    helper: &dyn PrivilegedBackend,
    guard: &SafetyGuard,
    plan: &Plan,
    decision: &Decision,
    cfg: &CleanupConfig,
    store: &Store,
    progress: &mut dyn FnMut(Progress),
    cancel: &CancelToken,
) -> ExecutionReport {
    use strata_clean::audit::ActionSummary as Summary;
    let mut summary = Summary::default();
    progress(Progress::Started {
        items: plan.items.len(),
        bytes: plan.total_bytes(),
    });
    let skipped_all = |reason: &dyn Fn(&QueueItem) -> CleanError| {
        plan.items
            .iter()
            .map(|i| ItemResult {
                id: i.id,
                path: i.path.display().to_string(),
                outcome: ItemOutcome::Skipped { reason: reason(i) },
            })
            .collect::<Vec<_>>()
    };
    if plan.items.is_empty() {
        progress(Progress::Finished { summary });
        return ExecutionReport {
            action: None,
            results: Vec::new(),
            summary,
        };
    }
    let mut log = StoreAuditLog::for_plan(store.clone(), plan, decision);
    let action = match log.begin_action(&AuditAction {
        method: DeleteMethod::Permanent,
        started_at: now_filetime(),
        items: plan.items.iter().map(audit_item).collect(),
    }) {
        Ok(a) => a,
        Err(e) => {
            let results = skipped_all(&|i| CleanError::AuditLogFailed {
                path: i.path.display().to_string(),
                message: e.0.clone(),
            });
            summary.skipped = results.len() as u64;
            progress(Progress::Finished { summary });
            return ExecutionReport {
                action: None,
                results,
                summary,
            };
        }
    };
    let mut results = Vec::with_capacity(plan.items.len());
    for item in &plan.items {
        let path = item.path.display().to_string();
        let outcome = if cancel.is_cancelled() {
            ItemOutcome::Skipped {
                reason: CleanError::Cancelled { path: path.clone() },
            }
        } else if let Err(reason) = elevated_gate(item, &decision.acks, cfg) {
            ItemOutcome::Skipped { reason }
        } else if let Err(e) = log.item_started(action, &audit_item(item)) {
            ItemOutcome::Skipped {
                reason: CleanError::AuditLogFailed {
                    path: path.clone(),
                    message: e.0,
                },
            }
        } else {
            progress(Progress::ItemStarted { id: item.id });
            let result = privileged_request(item).and_then(|req| {
                req.validate(guard)?;
                helper.delete_by_id(&req)
            });
            let outcome = match result {
                Ok(stats) => ItemOutcome::Deleted { stats },
                Err(error) => ItemOutcome::Failed { error },
            };
            let _ = log.item_finished(action, item.id, &outcome);
            progress(Progress::ItemFinished {
                id: item.id,
                removed: outcome.removed(),
            });
            outcome
        };
        if matches!(outcome, ItemOutcome::Skipped { .. }) {
            let _ = log.item_finished(action, item.id, &outcome);
        }
        match &outcome {
            o if o.removed() => {
                summary.succeeded += 1;
                summary.bytes += item.expected.size;
            }
            ItemOutcome::Failed { .. } => summary.failed += 1,
            _ => summary.skipped += 1,
        }
        results.push(ItemResult {
            id: item.id,
            path,
            outcome,
        });
    }
    summary.cancelled = cancel.is_cancelled();
    let _ = log.finish_action(action, &summary);
    progress(Progress::Finished { summary });
    ExecutionReport {
        action: Some(action),
        results,
        summary,
    }
}

// -----------------------------------------------------------------------------
// Restore and history
// -----------------------------------------------------------------------------

/// Restores one recycled item from the undo log and marks it restored.
///
/// # Errors
///
/// Unknown action/item, an item that is not restorable, or the restore's
/// own [`recycle::RestoreError`] (it never overwrites).
pub fn restore_item(store: &Store, action: i64, item: i64) -> FeatureResult<PathBuf> {
    let record = store.action(ActionId(action))?;
    let it = record
        .items
        .iter()
        .find(|i| i.id == ItemId(item))
        .ok_or_else(|| FeatureError::new(ErrorKind::NotFound, format!("item {item} not found")))?;
    if !is_restorable(it) {
        return Err(FeatureError::invalid(
            "this item was not moved to the Recycle Bin by Strata or was already restored",
        ));
    }
    let blob = &it
        .restore
        .as_ref()
        .map(|r| r.blob.clone())
        .unwrap_or_default();
    let restore_err =
        |e: recycle::RestoreError| FeatureError::with_detail(ErrorKind::Restore, e.to_string(), &e);
    let ticket = RestoreTicket::from_blob(blob).map_err(restore_err)?;
    let path = recycle::restore(&ticket).map_err(restore_err)?;
    store.mark_restored(it.id)?;
    Ok(path)
}

fn is_restorable(i: &ItemRecord) -> bool {
    i.planned.method == strata_store::DeleteMethod::Recycle
        && i.result == strata_store::ItemResult::Done
        && i.restored_at.is_none()
        && i.restore.as_ref().is_some_and(|r| !r.blob.is_empty())
}

fn ms(t: Timestamp) -> i64 {
    t.0.saturating_mul(1000)
}

/// One action in the cleanup history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionDto {
    /// Action id.
    pub id: i64,
    /// `cleanup`, `duplicates` or `tool`.
    pub kind: strata_store::ActionKind,
    /// `in_progress`, `completed`, `partial`, `failed`, `cancelled`, `interrupted`.
    pub status: strata_store::ActionStatus,
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
}

impl From<&ActionSummary> for ActionDto {
    fn from(s: &ActionSummary) -> Self {
        Self {
            id: s.id.0,
            kind: s.kind,
            status: s.status,
            started_ms: ms(s.started_at),
            finished_ms: s.finished_at.map(ms),
            item_count: s.item_count,
            done_count: s.done_count,
            failed_count: s.failed_count,
            bytes_done: s.bytes_done,
        }
    }
}

/// One logged item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionItemDto {
    /// Item id (for `cleanup_restore`).
    pub id: i64,
    /// Owning action id.
    pub action_id: i64,
    /// Position in the action.
    pub seq: u32,
    /// Path at the time.
    pub path: String,
    /// Bytes.
    pub size: u64,
    /// `recycle`, `permanent`, `reboot_delete` or `tool`.
    pub method: strata_store::DeleteMethod,
    /// Safety tier.
    pub tier: Safety,
    /// `pending`, `done`, `failed` or `skipped`.
    pub result: strata_store::ItemResult,
    /// Failure or skip reason.
    pub error: Option<String>,
    /// Unix ms.
    pub completed_ms: Option<i64>,
    /// Whether `cleanup_restore` can bring it back.
    pub restorable: bool,
    /// Unix ms, when restored.
    pub restored_ms: Option<i64>,
}

impl From<&ItemRecord> for ActionItemDto {
    fn from(i: &ItemRecord) -> Self {
        Self {
            id: i.id.0,
            action_id: i.action.0,
            seq: i.seq,
            path: i.planned.path.clone(),
            size: i.planned.size,
            method: i.planned.method,
            tier: i.planned.tier,
            result: i.result,
            error: i.error.clone(),
            completed_ms: i.completed_at.map(ms),
            restorable: is_restorable(i),
            restored_ms: i.restored_at.map(ms),
        }
    }
}

/// An action with its items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionDetailDto {
    /// Summary.
    pub action: ActionDto,
    /// Items in plan order.
    pub items: Vec<ActionItemDto>,
}

impl From<&ActionRecord> for ActionDetailDto {
    fn from(r: &ActionRecord) -> Self {
        Self {
            action: (&r.summary).into(),
            items: r.items.iter().map(Into::into).collect(),
        }
    }
}

// -----------------------------------------------------------------------------
// Tauri state and commands
// -----------------------------------------------------------------------------

/// Managed state wrapping the [`CleanupService`].
#[derive(Debug)]
pub struct CleanupState(Arc<CleanupService>);

impl CleanupState {
    /// Wraps a service.
    #[must_use]
    pub fn new(service: CleanupService) -> Self {
        Self(Arc::new(service))
    }

    /// The service.
    #[must_use]
    pub fn service(&self) -> Arc<CleanupService> {
        self.0.clone()
    }
}

/// Builds the guard for this machine. Strata's install folder and its data
/// folder are protected as subtrees in addition to the built-in list.
///
/// # Errors
///
/// When known folders or volumes cannot be read.
pub fn machine_guard(extra_protected: &[PathBuf]) -> FeatureResult<SafetyGuard> {
    let known = strata_win::known::known_folders().map_err(|e| {
        FeatureError::new(
            ErrorKind::Io,
            format!("could not resolve known folders: {e}"),
        )
    })?;
    let mut install_dirs: Vec<PathBuf> = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .into_iter()
        .collect();
    install_dirs.extend(extra_protected.iter().cloned());
    SafetyGuard::new(GuardConfig {
        known,
        install_dirs,
    })
    .map_err(|e| {
        FeatureError::new(
            ErrorKind::Io,
            format!("the safety guard could not start: {e}"),
        )
    })
}

fn service<R: Runtime>(app: &AppHandle<R>) -> FeatureResult<Arc<CleanupService>> {
    app.try_state::<CleanupState>()
        .map(|s| s.service())
        .ok_or_else(|| FeatureError::new(ErrorKind::NotReady, "cleanup is not initialized"))
}

/// Cleanup thresholds from the user's settings.
///
/// # Errors
///
/// Store errors.
pub fn config_from(store: &Store) -> FeatureResult<CleanupConfig> {
    let s = store.load_settings()?;
    Ok(CleanupConfig {
        large_permanent_bytes: s.cleanup.large_delete_confirm_bytes,
        ..CleanupConfig::default()
    })
}

/// Response of `cleanup_status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupStatus {
    /// Readiness.
    pub readiness: Readiness,
    /// Whether the elevated route is available.
    pub elevated_route: bool,
}

/// Whether cleanup can act, and whether the helper route is available.
#[tauri::command]
pub fn cleanup_status<R: Runtime>(app: AppHandle<R>) -> FeatureResult<CleanupStatus> {
    let s = service(&app)?;
    Ok(CleanupStatus {
        readiness: s.readiness(),
        elevated_route: s.privileged().is_some(),
    })
}

/// Builds and stores the review plan.
#[tauri::command]
pub async fn cleanup_plan<R: Runtime>(
    app: AppHandle<R>,
    items: Vec<QueueItem>,
) -> FeatureResult<PlanResponse> {
    let s = service(&app)?;
    blocking(move || s.plan(items)).await
}

/// Pre-flight (TOCTOU, locks, Recycle Bin fit) right before acting.
#[tauri::command]
pub async fn cleanup_preflight<R: Runtime>(
    app: AppHandle<R>,
    plan_id: u64,
    acks: Acknowledgements,
) -> FeatureResult<Vec<ItemVerdict>> {
    let s = service(&app)?;
    let store = super::store::handle(&app)?;
    let app2 = app.clone();
    blocking(move || {
        let cfg = config_from(&store)?;
        let verdicts = s.preflight(plan_id, &acks, &cfg)?;
        if let Some(h) = app2.try_state::<super::locks::KnownHolders>() {
            h.remember(verdicts.iter().flat_map(|v| v.holders.iter().cloned()));
        }
        Ok(verdicts)
    })
    .await
}

/// Executes a pre-flighted plan; progress streams over `on_progress`.
#[tauri::command]
pub async fn cleanup_execute<R: Runtime>(
    app: AppHandle<R>,
    plan_id: u64,
    decision: Decision,
    on_progress: Channel<Progress>,
) -> FeatureResult<ExecutionReport> {
    let s = service(&app)?;
    let store = super::store::handle(&app)?;
    blocking(move || {
        let cfg = config_from(&store)?;
        s.execute(plan_id, &decision, &cfg, &store, &mut |p| {
            let _ = on_progress.send(p);
        })
    })
    .await
}

/// Executes a pre-flighted plan through the elevated helper (permanent
/// deletes by file id).
#[tauri::command]
pub async fn cleanup_execute_elevated<R: Runtime>(
    app: AppHandle<R>,
    plan_id: u64,
    decision: Decision,
    on_progress: Channel<Progress>,
) -> FeatureResult<ExecutionReport> {
    let s = service(&app)?;
    let store = super::store::handle(&app)?;
    blocking(move || {
        let cfg = config_from(&store)?;
        s.execute_elevated(plan_id, &decision, &cfg, &store, &mut |p| {
            let _ = on_progress.send(p);
        })
    })
    .await
}

/// Cancels a running plan.
#[tauri::command]
pub fn cleanup_cancel<R: Runtime>(app: AppHandle<R>, plan_id: u64) -> FeatureResult<()> {
    service(&app)?.cancel(plan_id);
    Ok(())
}

/// A new plan of the retryable items of the last run.
#[tauri::command]
pub async fn cleanup_retry<R: Runtime>(
    app: AppHandle<R>,
    plan_id: u64,
) -> FeatureResult<PlanResponse> {
    let s = service(&app)?;
    blocking(move || s.retry(plan_id)).await
}

/// Forgets a plan.
#[tauri::command]
pub fn cleanup_discard<R: Runtime>(app: AppHandle<R>, plan_id: u64) -> FeatureResult<()> {
    service(&app)?.discard(plan_id);
    Ok(())
}

/// Undo history, newest first. Pass the last id of a page as `before`.
#[tauri::command]
pub async fn cleanup_history<R: Runtime>(
    app: AppHandle<R>,
    limit: Option<usize>,
    before: Option<i64>,
) -> FeatureResult<Vec<ActionDto>> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        let rows = store.action_history(limit.unwrap_or(50).min(500), before.map(ActionId))?;
        Ok(rows.iter().map(Into::into).collect())
    })
    .await
}

/// One action with its items.
#[tauri::command]
pub async fn cleanup_action<R: Runtime>(
    app: AppHandle<R>,
    action_id: i64,
) -> FeatureResult<ActionDetailDto> {
    let store = super::store::handle(&app)?;
    blocking(move || Ok((&store.action(ActionId(action_id))?).into())).await
}

/// Recycled items that can still be restored, newest first.
#[tauri::command]
pub async fn cleanup_restorable<R: Runtime>(
    app: AppHandle<R>,
    limit: Option<usize>,
) -> FeatureResult<Vec<ActionItemDto>> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        Ok(store
            .restorable_items(limit.unwrap_or(100).min(1000))?
            .iter()
            .map(Into::into)
            .collect())
    })
    .await
}

/// Response of `cleanup_restore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoredDto {
    /// Where the item is now.
    pub path: String,
}

/// Restores one recycled item to where it was (never overwrites).
#[tauri::command]
pub async fn cleanup_restore<R: Runtime>(
    app: AppHandle<R>,
    action_id: i64,
    item_id: i64,
) -> FeatureResult<RestoredDto> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        restore_item(&store, action_id, item_id).map(|p| RestoredDto {
            path: p.display().to_string(),
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use strata_clean::Expected;
    use strata_clean::recycle::RestoreTicket;
    use strata_core::FileRef;

    /// Files for one test, under `%TEMP%\strata-shell-tests\<random>`.
    /// Only this directory is ever written or removed.
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .join("strata-shell-tests")
                .join(super::super::consent::random_token().unwrap());
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, name: &str, body: &[u8]) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, body).unwrap();
            p
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_guard() -> FeatureResult<SafetyGuard> {
        let known = strata_win::known::known_folders()
            .map_err(|e| FeatureError::internal(e.to_string()))?;
        SafetyGuard::new(GuardConfig {
            known,
            install_dirs: Vec::new(),
        })
        .map_err(|e| FeatureError::internal(e.to_string()))
    }

    fn service() -> CleanupService {
        let s = CleanupService::new(Box::new(test_guard));
        s.set_ready();
        s
    }

    fn queue_item(guard: &SafetyGuard, id: u64, path: &Path) -> QueueItem {
        let checked = guard.check_path(path).unwrap();
        QueueItem {
            id,
            path: path.to_path_buf(),
            expected: Expected::from_facts(&checked.facts),
            safety: Safety::Safe,
        }
    }

    #[test]
    fn plan_preflight_execute_restore_round_trip() {
        let sandbox = Sandbox::new();
        let a = sandbox.file("a.tmp", b"alpha");
        let b = sandbox.file("b.tmp", b"bravo bravo");
        let guard = test_guard().unwrap();
        let items = vec![queue_item(&guard, 1, &a), queue_item(&guard, 2, &b)];
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::open(store_dir.path()).unwrap();
        let svc = service();
        let cfg = CleanupConfig::default();

        let planned = svc.plan(items).unwrap();
        assert_eq!(planned.plan.items.len(), 2);
        let decision = Decision::recycle();
        assert_eq!(
            svc.execute(planned.plan_id, &decision, &cfg, &store, &mut |_| {})
                .unwrap_err()
                .kind,
            ErrorKind::PreflightRequired
        );
        let verdicts = svc
            .preflight(planned.plan_id, &decision.acks, &cfg)
            .unwrap();
        assert!(
            verdicts
                .iter()
                .all(|v| matches!(v.verdict, strata_clean::preflight::Verdict::Ready { .. })),
            "{verdicts:?}"
        );

        let mut events = Vec::new();
        let report = svc
            .execute(planned.plan_id, &decision, &cfg, &store, &mut |p| {
                events.push(p)
            })
            .unwrap();
        let tickets: Vec<RestoreTicket> = report
            .results
            .iter()
            .filter_map(|r| match &r.outcome {
                ItemOutcome::Recycled { ticket } => Some(ticket.clone()),
                _ => None,
            })
            .collect();
        // If anything below fails, put our own two files back first.
        let restore_guard = RestoreOnDrop(tickets.clone());
        assert_eq!(report.summary.succeeded, 2, "{report:?}");
        assert!(!a.exists() && !b.exists());
        assert!(matches!(
            events.first(),
            Some(Progress::Started { items: 2, .. })
        ));
        assert!(matches!(events.last(), Some(Progress::Finished { .. })));

        // The undo log has both items, restorable.
        let history = store.action_history(10, None).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, strata_store::ActionStatus::Completed);
        let record = store.action(history[0].id).unwrap();
        assert!(record.items.iter().all(is_restorable));

        // Restore exactly those items through the log.
        for item in &record.items {
            let back = restore_item(&store, record.summary.id.0, item.id.0).unwrap();
            assert!(back.exists());
        }
        std::mem::forget(restore_guard);
        assert_eq!(std::fs::read(&a).unwrap(), b"alpha");
        assert_eq!(std::fs::read(&b).unwrap(), b"bravo bravo");
        assert!(store.restorable_items(10).unwrap().is_empty());
        let again = restore_item(&store, record.summary.id.0, record.items[0].id.0);
        assert_eq!(again.unwrap_err().kind, ErrorKind::InvalidInput);

        // The plan needs a fresh pre-flight to run again.
        assert_eq!(
            svc.execute(planned.plan_id, &decision, &cfg, &store, &mut |_| {})
                .unwrap_err()
                .kind,
            ErrorKind::PreflightRequired
        );
    }

    /// Restores recycled test files if an assertion fails mid-test, so the
    /// real Recycle Bin never keeps anything a test put there.
    struct RestoreOnDrop(Vec<RestoreTicket>);

    impl Drop for RestoreOnDrop {
        fn drop(&mut self) {
            for t in &self.0 {
                let _ = recycle::restore(t);
            }
        }
    }

    #[test]
    fn never_list_is_enforced_at_the_app_layer() {
        let guard = test_guard().unwrap();
        let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let protected = QueueItem {
            id: 7,
            path: PathBuf::from(&windir),
            expected: Expected {
                file_ref: FileRef(5),
                is_dir: true,
                size: 1,
                modified: FileTime(0),
            },
            safety: Safety::Safe,
        };
        let (kept, refused) = enforce_never_list(&guard, vec![protected.clone()]);
        assert!(kept.is_empty());
        assert!(matches!(refused[0], PlanWarning::Refused { id: 7, .. }));

        let svc = service();
        let planned = svc.plan(vec![protected]).unwrap();
        assert!(planned.plan.items.is_empty());
        assert!(
            planned
                .plan
                .warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::Refused { id: 7, .. }))
        );
    }

    #[test]
    fn cleanup_waits_for_recovery() {
        let svc = CleanupService::new(Box::new(test_guard));
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::open(store_dir.path()).unwrap();
        let err = svc
            .execute(
                1,
                &Decision::recycle(),
                &CleanupConfig::default(),
                &store,
                &mut |_| {},
            )
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotReady);
        svc.set_unavailable("state.db is damaged".into());
        assert!(matches!(svc.readiness(), Readiness::Unavailable { .. }));
    }

    #[test]
    fn unknown_plans_are_rejected() {
        let svc = service();
        assert_eq!(
            svc.preflight(99, &Acknowledgements::default(), &CleanupConfig::default())
                .unwrap_err()
                .kind,
            ErrorKind::UnknownPlan
        );
        assert_eq!(svc.retry(99).unwrap_err().kind, ErrorKind::UnknownPlan);
    }

    #[test]
    fn elevated_gate_requires_every_confirmation() {
        let cfg = CleanupConfig {
            large_permanent_bytes: 100,
            ..CleanupConfig::default()
        };
        let item = |safety, size| QueueItem {
            id: 1,
            path: r"D:\x\y".into(),
            expected: Expected {
                file_ref: FileRef(1),
                is_dir: false,
                size,
                modified: FileTime(0),
            },
            safety,
        };
        let mut acks = Acknowledgements::default();
        assert!(matches!(
            elevated_gate(&item(Safety::Never, 1), &acks, &cfg),
            Err(CleanError::NeverTier { .. })
        ));
        assert!(matches!(
            elevated_gate(&item(Safety::Safe, 1), &acks, &cfg),
            Err(CleanError::NeedsPermanentConfirmation { .. })
        ));
        acks.permanent = true;
        assert!(elevated_gate(&item(Safety::Safe, 1), &acks, &cfg).is_ok());
        assert!(matches!(
            elevated_gate(&item(Safety::Careful, 1), &acks, &cfg),
            Err(CleanError::NeedsAcknowledgement { .. })
        ));
        acks.careful.insert(1);
        assert!(elevated_gate(&item(Safety::Careful, 1), &acks, &cfg).is_ok());
        assert!(matches!(
            elevated_gate(&item(Safety::Safe, 101), &acks, &cfg),
            Err(CleanError::NeedsLargeDeleteConfirmation { .. })
        ));
        acks.large_permanent = true;
        assert!(elevated_gate(&item(Safety::Safe, 101), &acks, &cfg).is_ok());
    }

    struct RecordingHelper(Mutex<Vec<PrivilegedDeleteRequest>>);

    impl PrivilegedBackend for RecordingHelper {
        fn delete_by_id(
            &self,
            request: &PrivilegedDeleteRequest,
        ) -> Result<DeleteStats, CleanError> {
            lock(&self.0).push(request.clone());
            // Simulate the helper refusing, so nothing on disk changes.
            Err(CleanError::AccessDenied {
                path: request.expected_path.clone(),
            })
        }
    }

    #[test]
    fn elevated_route_sends_validated_requests_and_logs_them() {
        let sandbox = Sandbox::new();
        let f = sandbox.file("c.tmp", b"charlie");
        let guard = test_guard().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::open(store_dir.path()).unwrap();
        let svc = service();
        let cfg = CleanupConfig::default();
        let planned = svc.plan(vec![queue_item(&guard, 3, &f)]).unwrap();
        let decision = Decision {
            method: DeleteMethod::Permanent,
            acks: Acknowledgements {
                permanent: true,
                ..Acknowledgements::default()
            },
        };
        assert_eq!(
            svc.execute_elevated(planned.plan_id, &decision, &cfg, &store, &mut |_| {})
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported,
            "no helper connected"
        );
        let helper = Arc::new(RecordingHelper(Mutex::new(Vec::new())));
        svc.set_privileged(Some(helper.clone()));
        assert_eq!(
            svc.execute_elevated(
                planned.plan_id,
                &Decision::recycle(),
                &cfg,
                &store,
                &mut |_| {}
            )
            .unwrap_err()
            .kind,
            ErrorKind::InvalidInput,
            "the helper never recycles"
        );
        svc.preflight(planned.plan_id, &decision.acks, &cfg)
            .unwrap();
        let report = svc
            .execute_elevated(planned.plan_id, &decision, &cfg, &store, &mut |_| {})
            .unwrap();
        assert_eq!(report.summary.failed, 1);
        let sent = lock(&helper.0);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].expected_path, f.display().to_string());
        assert!(f.exists());
        let rec = store
            .action(store.action_history(1, None).unwrap()[0].id)
            .unwrap();
        assert_eq!(rec.summary.status, strata_store::ActionStatus::Failed);
        assert_eq!(
            rec.items[0].planned.method,
            strata_store::DeleteMethod::Permanent
        );
    }
}
