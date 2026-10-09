//! Analysis commands (`ui/src/lib/insights.ts`): largest entries, file
//! types, categories, apps, orphans and recommendations.

use std::sync::Arc;

use strata_core::SizeMode;
use tauri::{AppHandle, Manager, State};

use super::{blocking, with_data};
use crate::error::{CmdResult, CommandError};
use crate::features::queue::{QueueAddResult, QueueSource, add_to_queue, candidates};
use crate::insights::{
    self, AppFootprint, CategoryTotal, FileTypeBreakdown, LargestQuery, LargestResult,
    OrphanFolder, Recommendation, RecommendationCache, RecommendationConfig, RecommendationItem,
};
use crate::state::AppState;

/// Global top-N (`insights_largest`).
///
/// # Errors
///
/// Unknown volume or scope.
#[tauri::command]
pub async fn insights_largest(
    state: State<'_, Arc<AppState>>,
    query: LargestQuery,
) -> CmdResult<LargestResult> {
    let st = state.inner().clone();
    blocking(move || {
        let engine = st.engine.get();
        with_data(&st, &query.volume_id, |d| {
            insights::largest(d, engine.as_deref(), &query)
        })
    })
    .await
}

/// Extension and detected-type breakdown (`insights_file_types`).
///
/// # Errors
///
/// Unknown volume or scope.
#[tauri::command]
pub async fn insights_file_types(
    state: State<'_, Arc<AppState>>,
    volume_id: String,
    scope: Option<u32>,
    size_mode: SizeMode,
) -> CmdResult<FileTypeBreakdown> {
    let st = state.inner().clone();
    blocking(move || {
        with_data(&st, &volume_id, |d| {
            insights::file_types(d, scope, size_mode)
        })
    })
    .await
}

/// Totals per category (`insights_categories`).
///
/// # Errors
///
/// Unknown volume or scope.
#[tauri::command]
pub async fn insights_categories(
    state: State<'_, Arc<AppState>>,
    volume_id: String,
    scope: Option<u32>,
    size_mode: SizeMode,
) -> CmdResult<Vec<CategoryTotal>> {
    let st = state.inner().clone();
    blocking(move || {
        with_data(&st, &volume_id, |d| {
            insights::categories(d, scope, size_mode)
        })
    })
    .await
}

/// Apps with their measured footprint (`apps_footprint`).
///
/// # Errors
///
/// Never; an empty list until a volume is scanned.
#[tauri::command]
pub async fn apps_footprint(state: State<'_, Arc<AppState>>) -> CmdResult<Vec<AppFootprint>> {
    let st = state.inner().clone();
    blocking(move || Ok(insights::footprints(&st))).await
}

/// Orphaned app data (`apps_orphans`).
///
/// # Errors
///
/// Never; an empty list until a volume is scanned.
#[tauri::command]
pub async fn apps_orphans(state: State<'_, Arc<AppState>>) -> CmdResult<Vec<OrphanFolder>> {
    let st = state.inner().clone();
    blocking(move || Ok(insights::orphans(&st))).await
}

/// Queues an app's safe/probably cache locations (`apps_queue_caches`).
///
/// # Errors
///
/// Unknown app, or the safety guard could not start.
#[tauri::command]
pub async fn apps_queue_caches(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    app_id: String,
) -> CmdResult<QueueAddResult> {
    let st = state.inner().clone();
    blocking(move || {
        let fp = insights::footprints(&st)
            .into_iter()
            .find(|a| a.id == app_id)
            .ok_or_else(|| CommandError::not_found("that app is no longer in the catalog"))?;
        let mut cands = Vec::new();
        let mut refused = Vec::new();
        for l in fp.locations.iter().filter(|l| {
            l.kind == insights::FootprintKind::Cache
                && matches!(l.safety, Some("safe" | "probably"))
        }) {
            let (mut c, mut r) = candidates(
                &st,
                &l.volume_id,
                &l.entry_id.into_iter().collect::<Vec<_>>(),
            );
            cands.append(&mut c);
            refused.append(&mut r);
        }
        add_to_queue(&app, cands, refused, QueueSource::AppCaches)
    })
    .await
}

fn config(st: &AppState) -> RecommendationConfig {
    let s = st
        .store()
        .and_then(|s| s.load_settings().ok())
        .unwrap_or_default();
    let (bytes, items) = crate::features::tools::recycle_bin_totals();
    let dupes = st.dupes.status();
    RecommendationConfig {
        stale_node_modules_days: s.cleanup.stale_node_modules_days,
        stale_installers_days: s.cleanup.stale_installers_days,
        recycle_bin_bytes: bytes,
        recycle_bin_items: items,
        duplicate_bytes: dupes.wasted_bytes,
        duplicate_groups: dupes.groups,
    }
}

/// Ranked recommendations (`recommendations_list`).
///
/// # Errors
///
/// Never; an empty list until a volume is scanned.
#[tauri::command]
pub async fn recommendations_list(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> CmdResult<Vec<Recommendation>> {
    let st = state.inner().clone();
    blocking(move || {
        let cfg = config(&st);
        let cache = app.state::<RecommendationCache>();
        Ok(insights::recommendations(&st, &cache, &cfg))
    })
    .await
}

/// Preview of a recommendation's items.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecommendationPreview {
    /// The first `limit` items, largest first.
    pub items: Vec<RecommendationItem>,
    /// All items.
    pub total: u64,
}

fn rec_items(app: &AppHandle, id: &str) -> CmdResult<Vec<RecommendationItem>> {
    app.state::<RecommendationCache>().items(id).ok_or_else(|| {
        CommandError::not_found("that recommendation is out of date; refresh the list")
    })
}

/// The items a recommendation covers (`recommendations_preview`).
///
/// # Errors
///
/// An unknown or outdated recommendation id.
#[tauri::command]
pub fn recommendations_preview(
    app: AppHandle,
    id: String,
    limit: usize,
) -> CmdResult<RecommendationPreview> {
    let mut items = rec_items(&app, &id)?;
    items.sort_by_key(|x| std::cmp::Reverse(x.bytes));
    let total = items.len() as u64;
    items.truncate(limit.clamp(1, 10_000));
    Ok(RecommendationPreview { items, total })
}

/// Queues a recommendation's items except `exclude` (entry ids)
/// (`recommendations_queue`).
///
/// # Errors
///
/// An unknown recommendation, or the safety guard could not start.
#[tauri::command]
pub async fn recommendations_queue(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    id: String,
    exclude: Vec<u32>,
) -> CmdResult<QueueAddResult> {
    let st = state.inner().clone();
    blocking(move || {
        let items = rec_items(&app, &id)?;
        let mut by_volume: std::collections::BTreeMap<String, Vec<u32>> = Default::default();
        for i in items.iter().filter(|i| !exclude.contains(&i.entry_id)) {
            by_volume
                .entry(i.volume_id.clone())
                .or_default()
                .push(i.entry_id);
        }
        let mut cands = Vec::new();
        let mut refused = Vec::new();
        for (v, ids) in by_volume {
            let (mut c, mut r) = candidates(&st, &v, &ids);
            cands.append(&mut c);
            refused.append(&mut r);
        }
        add_to_queue(&app, cands, refused, QueueSource::Recommendation)
    })
    .await
}
