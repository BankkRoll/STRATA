//! Analysis views over the indexes, the classifier and the app catalog:
//! largest entries, file types, categories, app footprints, orphaned app
//! data and the ranked "Free up space" recommendations
//! (`ui/src/lib/insights.ts`).
//!
//! Everything is computed on demand from the published indexes (read lock
//! held for one query). Recommendations are remembered with their items so
//! a preview and a "queue it" act on exactly what the list showed.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use strata_classify::catalog::{AppSource, Confidence};
use strata_core::{Category, EntryFlags, EpochSecs, Safety, SizeMode};
use strata_index::{EntryId, EntryKind};

use crate::classify::Engine;
use crate::error::{CmdResult, CommandError};
use crate::model::{VolumeData, epoch2000_to_ms, now_epoch2000};
use crate::state::{AppState, read};

// -----------------------------------------------------------------------------
// Shared
// -----------------------------------------------------------------------------

/// Every volume with a published, complete-or-partial index.
fn indexed(state: &AppState) -> Vec<(String, Arc<crate::state::Session>)> {
    let ids: Vec<String> = crate::state::lock(&state.sessions)
        .keys()
        .cloned()
        .collect();
    ids.into_iter()
        .filter_map(|id| {
            let s = state.existing_session(&id)?;
            read(&s.data).as_ref().filter(|d| !d.preview)?;
            Some((id, s))
        })
        .collect()
}

fn safety_name(s: Safety) -> &'static str {
    match s {
        Safety::Safe => "safe",
        Safety::Probably => "probably",
        Safety::Careful => "careful",
        Safety::Never => "never",
    }
}

/// The tier an entry shows: its rule's, or `None` when no rule claims it
/// (the never default always shows).
fn shown_safety(data: &VolumeData, id: EntryId) -> Option<Safety> {
    match crate::classify::safety_code(data.class_bits(id)) {
        1 => Some(Safety::Safe),
        2 => Some(Safety::Probably),
        3 => Some(Safety::Careful),
        4 => Some(Safety::Never),
        _ => None,
    }
}

fn resolve_scope(data: &VolumeData, scope: Option<u32>) -> CmdResult<EntryId> {
    match scope {
        None => Ok(data.index.root()),
        Some(w) => data
            .resolve(w)
            .ok_or_else(|| CommandError::not_found("that folder is no longer in the index")),
    }
}

// -----------------------------------------------------------------------------
// Largest
// -----------------------------------------------------------------------------

/// Filters of the largest-entries query (`LargestFilters` in the UI).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LargestFilters {
    /// Lowercase extensions without the dot; empty = any.
    pub extensions: Vec<String>,
    /// Category ids; empty = any.
    pub categories: Vec<u16>,
    /// Safety tiers; empty = any.
    pub safety: Vec<Safety>,
    /// Smallest size.
    pub min_bytes: u64,
    /// Only entries modified within N days.
    pub modified_within_days: Option<u32>,
    /// Only entries untouched for at least N days.
    pub untouched_for_days: Option<u32>,
}

/// Files or folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LargestKind {
    /// Files.
    Files,
    /// Folders.
    Folders,
}

/// `LargestQuery` in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LargestQuery {
    /// Volume.
    pub volume_id: String,
    /// Folder to search under; `None` = the whole volume.
    pub scope: Option<u32>,
    /// Files or folders.
    pub kind: LargestKind,
    /// At most this many (capped at 10,000).
    pub limit: usize,
    /// Size mode.
    pub size_mode: SizeMode,
    /// Filters.
    #[serde(default)]
    pub filters: LargestFilters,
}

/// One ranked entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LargestEntry {
    /// Wire id.
    pub id: u32,
    /// Name.
    pub name: String,
    /// Path.
    pub path: String,
    /// Size in the requested mode.
    pub bytes: u64,
    /// Modified (Unix ms).
    pub modified_ms: Option<i64>,
    /// Category id.
    pub category: u16,
    /// Tier, when a rule claims it.
    pub safety: Option<&'static str>,
    /// Owning app.
    pub app: Option<String>,
    /// Lowercase extension.
    pub extension: Option<String>,
}

/// Result of [`largest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LargestResult {
    /// Largest first.
    pub entries: Vec<LargestEntry>,
    /// Entries matching the filters.
    pub matched: u64,
}

fn entry_dto(data: &VolumeData, engine: Option<&Engine>, id: EntryId, bytes: u64) -> LargestEntry {
    let ix = &data.index;
    LargestEntry {
        id: data.wire(id),
        name: ix.name_lossy(id),
        path: data.path(id),
        bytes,
        modified_ms: epoch2000_to_ms(data.modified(id)),
        category: (data.key_static(id) & 0xF) as u16,
        safety: shown_safety(data, id).map(safety_name),
        app: engine.and_then(|e| e.apps.name(ix.owner_app(id))),
        extension: (!ix.is_dir(id)).then(|| ix.extension(id)).flatten(),
    }
}

/// The largest files or folders under the scope that pass the filters.
///
/// # Errors
///
/// An unknown scope.
pub fn largest(
    data: &VolumeData,
    engine: Option<&Engine>,
    q: &LargestQuery,
) -> CmdResult<LargestResult> {
    let scope = resolve_scope(data, q.scope)?;
    let ix = &data.index;
    let limit = q.limit.clamp(1, 10_000);
    let exts: Vec<u16> = q
        .filters
        .extensions
        .iter()
        .filter_map(|e| ix.extension_id(e.trim_start_matches('.')))
        .collect();
    if !q.filters.extensions.is_empty() && exts.is_empty() {
        return Ok(LargestResult {
            entries: Vec::new(),
            matched: 0,
        });
    }
    let now = now_epoch2000();
    let since = q
        .filters
        .modified_within_days
        .map(|d| now.saturating_sub(d.saturating_mul(86_400)));
    let before = q
        .filters
        .untouched_for_days
        .map(|d| now.saturating_sub(d.saturating_mul(86_400)));
    let want_dirs = q.kind == LargestKind::Folders;
    let mut heap: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::with_capacity(limit + 1);
    let mut matched = 0u64;
    ix.for_each_in_subtree(scope, |id| {
        if id == scope || ix.is_dir(id) != want_dirs {
            return;
        }
        if ix.flags(id).contains(EntryFlags::VIRTUAL) {
            return;
        }
        let bytes = ix.size(id, q.size_mode);
        if bytes < q.filters.min_bytes {
            return;
        }
        if !exts.is_empty() && !exts.contains(&ix.ext_id(id)) {
            return;
        }
        if !q.filters.categories.is_empty()
            && !q
                .filters
                .categories
                .contains(&((data.key_static(id) & 0xF) as u16))
        {
            return;
        }
        let m = data.modified(id);
        if since.is_some_and(|s| m < s) || before.is_some_and(|b| m == 0 || m > b) {
            return;
        }
        if !q.filters.safety.is_empty()
            && !shown_safety(data, id).is_some_and(|s| q.filters.safety.contains(&s))
        {
            return;
        }
        matched += 1;
        heap.push(Reverse((bytes, id.0)));
        if heap.len() > limit {
            heap.pop();
        }
    });
    let mut top: Vec<(u64, u32)> = heap.into_iter().map(|Reverse(x)| x).collect();
    top.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    Ok(LargestResult {
        entries: top
            .into_iter()
            .map(|(bytes, id)| entry_dto(data, engine, EntryId(id), bytes))
            .collect(),
        matched,
    })
}

// -----------------------------------------------------------------------------
// File types
// -----------------------------------------------------------------------------

/// Display group of an extension.
#[must_use]
pub fn extension_group(ext: &str) -> &'static str {
    match ext {
        "" => "No extension",
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" | "mpg" | "mpeg" | "ts" | "flv" => {
            "Video"
        }
        "mp3" | "flac" | "wav" | "aac" | "ogg" | "m4a" | "wma" | "opus" => "Audio",
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "tif" | "tiff" | "raw"
        | "cr2" | "nef" | "arw" | "dng" | "psd" | "svg" | "ico" => "Image",
        "zip" | "7z" | "rar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "tar" | "cab" | "lz4" => {
            "Archive"
        }
        "iso" | "img" | "vhd" | "vhdx" | "vmdk" | "vdi" | "qcow2" | "wim" | "esd" => "Disk image",
        "exe" | "msi" | "msix" | "msixbundle" | "appx" | "appxbundle" | "dll" | "sys" | "so"
        | "dylib" => "Program",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "txt"
        | "rtf" | "md" | "csv" | "epub" => "Document",
        "gguf" | "safetensors" | "ckpt" | "pt" | "pth" | "onnx" | "bin" | "h5" | "tflite" => {
            "Model or data"
        }
        "db" | "sqlite" | "sqlite3" | "mdb" | "accdb" | "ldf" | "mdf" | "ndf" | "pst" | "ost" => {
            "Database"
        }
        "log" | "etl" | "dmp" | "evtx" => "Log or dump",
        "rs" | "c" | "cpp" | "h" | "hpp" | "cs" | "java" | "js" | "tsx" | "py" | "go" | "json"
        | "xml" | "yaml" | "yml" | "toml" | "html" | "css" => "Code",
        "pak" | "vpk" | "bsa" | "ba2" | "uasset" | "assets" | "bundle" => "Game data",
        "rlib" | "rmeta" | "pdb" | "lib" | "a" | "o" | "obj" | "pch" | "ilk" | "exp" | "d" => {
            "Build output"
        }
        "tmp" | "temp" | "bak" | "old" | "cache" => "Temporary",
        _ => "Other",
    }
}

/// One extension's totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExtensionRow {
    /// Lowercase extension ("" for none).
    pub extension: String,
    /// Display group.
    pub group: &'static str,
    /// Files.
    pub files: u64,
    /// Bytes.
    pub bytes: u64,
    /// Sniffed files whose content disagrees with the extension.
    pub mismatched: u64,
}

/// Totals by sniffed content type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DetectedTypeRow {
    /// Label, e.g. "ZIP archive".
    pub label: String,
    /// Files.
    pub files: u64,
    /// Bytes.
    pub bytes: u64,
}

/// `FileTypeBreakdown` in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTypeBreakdown {
    /// By extension, largest first.
    pub by_extension: Vec<ExtensionRow>,
    /// By sniffed content (the largest local files only).
    pub by_detected_type: Vec<DetectedTypeRow>,
    /// Total bytes under the scope.
    pub total_bytes: u64,
}

/// How many of the largest files are sniffed for the content table.
const SNIFF_FILES: usize = 300;

/// Extension totals plus a content sniff of the largest plain local files.
///
/// Content is only read from local files: cloud placeholders, offline
/// files and reparse points are never opened, so this cannot trigger a
/// download.
///
/// # Errors
///
/// An unknown scope.
pub fn file_types(
    data: &VolumeData,
    scope: Option<u32>,
    mode: SizeMode,
) -> CmdResult<FileTypeBreakdown> {
    use strata_classify::sniff;
    let scope = resolve_scope(data, scope)?;
    let ix = &data.index;
    let breakdown = ix.extension_breakdown(Some(scope), mode);
    let total_bytes = ix.size(scope, mode);
    let mut mismatched: HashMap<String, u64> = HashMap::new();
    let mut detected: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let filter = strata_index::Filter {
        kind: EntryKind::Files,
        flags_none: EntryFlags::OFFLINE,
        ..strata_index::Filter::default()
    };
    for (id, bytes) in ix.top_n(
        Some(scope),
        SNIFF_FILES,
        EntryKind::Files,
        mode,
        Some(&filter),
    ) {
        let f = ix.flags(id);
        if f.cloud() != strata_core::CloudState::None
            || f.reparse() != strata_core::ReparseKind::None
            || f.contains(EntryFlags::VIRTUAL)
        {
            continue;
        }
        let path = data.path(id);
        let Some((h, t)) = crate::shell::read_head_tail(
            std::path::Path::new(&path),
            sniff::HEAD_LEN,
            sniff::TAIL_LEN,
        ) else {
            continue;
        };
        let Some(d) = sniff::sniff_with_tail(&h, (!t.is_empty()).then_some(&t[..])) else {
            continue;
        };
        let e = detected.entry(d.label().to_owned()).or_default();
        e.0 += 1;
        e.1 += bytes;
        if sniff::mismatch(&ix.name_lossy(id), d).is_some() {
            *mismatched
                .entry(ix.extension(id).unwrap_or_default())
                .or_default() += 1;
        }
    }
    let mut by_detected_type: Vec<DetectedTypeRow> = detected
        .into_iter()
        .map(|(label, (files, bytes))| DetectedTypeRow {
            label,
            files,
            bytes,
        })
        .collect();
    by_detected_type.sort_by_key(|x| std::cmp::Reverse(x.bytes));
    Ok(FileTypeBreakdown {
        by_extension: breakdown
            .into_iter()
            .map(|b| ExtensionRow {
                group: extension_group(&b.label),
                mismatched: mismatched.get(&b.label).copied().unwrap_or(0),
                extension: b.label,
                files: b.files,
                bytes: b.bytes,
            })
            .collect(),
        by_detected_type,
        total_bytes,
    })
}

// -----------------------------------------------------------------------------
// Categories
// -----------------------------------------------------------------------------

/// A top contributor of a category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Contributor {
    /// Wire id.
    pub id: u32,
    /// Name.
    pub name: String,
    /// Path.
    pub path: String,
    /// Bytes.
    pub bytes: u64,
}

/// One category's totals (`CategoryTotal` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CategoryTotal {
    /// Category id.
    pub category: u16,
    /// Bytes.
    pub bytes: u64,
    /// Files.
    pub files: u64,
    /// Largest entries of the category.
    pub top: Vec<Contributor>,
}

/// Totals per category under the scope, with the largest files of each.
///
/// # Errors
///
/// An unknown scope.
pub fn categories(
    data: &VolumeData,
    scope: Option<u32>,
    mode: SizeMode,
) -> CmdResult<Vec<CategoryTotal>> {
    let scope = resolve_scope(data, scope)?;
    let ix = &data.index;
    Ok(ix
        .category_breakdown(Some(scope), mode)
        .into_iter()
        .filter(|b| b.bytes > 0)
        .map(|b| {
            let cat = u16::try_from(b.id).unwrap_or(0);
            let filter = strata_index::Filter {
                kind: EntryKind::Files,
                categories: vec![cat],
                ..strata_index::Filter::default()
            };
            let top = ix
                .top_n(Some(scope), 5, EntryKind::Files, mode, Some(&filter))
                .into_iter()
                .map(|(id, bytes)| Contributor {
                    id: data.wire(id),
                    name: ix.name_lossy(id),
                    path: data.path(id),
                    bytes,
                })
                .collect();
            CategoryTotal {
                category: cat,
                bytes: b.bytes,
                files: b.files,
                top,
            }
        })
        .collect())
}

// -----------------------------------------------------------------------------
// Apps
// -----------------------------------------------------------------------------

/// Kind of a footprint location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FootprintKind {
    /// Program files.
    Install,
    /// User or app data.
    Data,
    /// Caches.
    Cache,
    /// Logs and dumps.
    Logs,
    /// Update downloads.
    Updates,
    /// Anything else attributed to it.
    Other,
}

/// One location of an app's footprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FootprintLocation {
    /// Kind.
    pub kind: FootprintKind,
    /// Path.
    pub path: String,
    /// Volume.
    pub volume_id: String,
    /// Wire id.
    pub entry_id: Option<u32>,
    /// Allocated bytes.
    pub bytes: u64,
    /// Tier.
    pub safety: Option<&'static str>,
}

/// An app and everything attributed to it (`AppFootprint` in the UI).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppFootprint {
    /// Stable id: the catalog key, or `label:<name>` for rule labels.
    pub id: String,
    /// Name.
    pub name: String,
    /// Publisher.
    pub publisher: Option<String>,
    /// Version.
    pub version: Option<String>,
    /// `registry`, `appx`, `launcher`, `rule` or `heuristic`.
    pub source: &'static str,
    /// Strongest attribution confidence.
    pub confidence: &'static str,
    /// Evidence lines.
    pub evidence: Vec<String>,
    /// Measured bytes.
    pub total_bytes: u64,
    /// Locations, largest first.
    pub locations: Vec<FootprintLocation>,
    /// Registry `EstimatedSize`.
    pub registry_estimate_bytes: Option<u64>,
    /// Measured and registry sizes differ by more than 4x (both above
    /// 100 MiB).
    pub mismatch: bool,
    /// An uninstall command exists.
    pub can_uninstall: bool,
    /// Bytes in safe/probably cache locations.
    pub cache_bytes: u64,
    /// A process owning one of its caches runs now.
    pub running: bool,
}

fn location_kind(category: Category, path: &str, under_install: bool) -> FootprintKind {
    if under_install {
        return FootprintKind::Install;
    }
    let lower = path.to_lowercase();
    match category {
        Category::Caches | Category::Temp => FootprintKind::Cache,
        Category::Apps | Category::Games => FootprintKind::Install,
        _ if lower.ends_with("\\logs") || lower.contains("\\logs\\") => FootprintKind::Logs,
        _ if lower.contains("update") => FootprintKind::Updates,
        _ if lower.contains("\\appdata\\") || lower.contains("\\programdata\\") => {
            FootprintKind::Data
        }
        _ => FootprintKind::Other,
    }
}

fn path_key(p: &str) -> String {
    p.trim_end_matches('\\').to_lowercase()
}

/// The stable id of catalog app `i`.
#[must_use]
pub fn app_id(app: &strata_classify::catalog::InstalledApp) -> String {
    app.key.clone()
}

fn confidence_name(c: Confidence) -> &'static str {
    match c {
        Confidence::Exact => "exact",
        Confidence::High => "high",
        Confidence::Heuristic => "heuristic",
    }
}

/// Every app with its measured footprint across the indexed volumes.
#[must_use]
pub fn footprints(state: &AppState) -> Vec<AppFootprint> {
    let Some(engine) = state.engine.get() else {
        return Vec::new();
    };
    // Where attribution starts in each index: an entry whose owner differs
    // from its parent's. Those subtrees are the app's locations.
    let mut by_label: HashMap<u32, Vec<FootprintLocation>> = HashMap::new();
    let catalog = engine.catalog.get();
    let install_roots: Vec<(String, usize)> = catalog
        .as_ref()
        .map(|c| {
            c.apps()
                .iter()
                .enumerate()
                .filter_map(|(i, a)| {
                    a.install_location
                        .as_ref()
                        .map(|p| (path_key(&p.display().to_string()), i))
                })
                .collect()
        })
        .unwrap_or_default();
    for (volume_id, session) in indexed(state) {
        let g = read(&session.data);
        let Some(data) = g.as_ref() else { continue };
        let ix = &data.index;
        ix.for_each_in_subtree(ix.root(), |id| {
            let owner = ix.owner_app(id);
            if owner == 0 || !ix.is_dir(id) {
                return;
            }
            if ix.parent(id).is_some_and(|p| ix.owner_app(p) == owner) {
                return;
            }
            let path = data.path(id);
            let key = path_key(&path);
            let under_install = install_roots
                .iter()
                .any(|(r, _)| key == *r || key.starts_with(&format!("{r}\\")));
            let cat = data.class(id).map_or(Category::Unknown, |c| c.category);
            by_label.entry(owner).or_default().push(FootprintLocation {
                kind: location_kind(cat, &path, under_install),
                path,
                volume_id: volume_id.clone(),
                entry_id: Some(data.wire(id)),
                bytes: ix.size(id, SizeMode::Allocated),
                safety: shown_safety(data, id).map(safety_name),
            });
        });
    }
    let mut out = Vec::new();
    let mut used_labels = std::collections::HashSet::new();
    if let Some(cat) = catalog {
        for app in cat.apps().iter().filter(|a| !a.system_component) {
            let label = engine
                .apps
                .all()
                .into_iter()
                .find(|(_, n)| n == &app.name)
                .map(|(id, _)| id);
            let mut locations = label
                .and_then(|l| {
                    used_labels.insert(l);
                    by_label.get(&l).cloned()
                })
                .unwrap_or_default();
            if locations.is_empty() && app.estimated_size.is_none() {
                continue;
            }
            locations.sort_by_key(|x| std::cmp::Reverse(x.bytes));
            out.push(footprint(
                &engine,
                app_id(app),
                app.name.clone(),
                Some(app),
                locations,
            ));
        }
    }
    for (label, mut locations) in by_label {
        if used_labels.contains(&label) {
            continue;
        }
        let Some(name) = engine.apps.name(label) else {
            continue;
        };
        locations.sort_by_key(|x| std::cmp::Reverse(x.bytes));
        out.push(footprint(
            &engine,
            format!("label:{name}"),
            name,
            None,
            locations,
        ));
    }
    out.sort_by_key(|x| std::cmp::Reverse(x.total_bytes));
    out
}

fn footprint(
    engine: &Engine,
    id: String,
    name: String,
    app: Option<&strata_classify::catalog::InstalledApp>,
    locations: Vec<FootprintLocation>,
) -> AppFootprint {
    let total_bytes: u64 = locations.iter().map(|l| l.bytes).sum();
    let cache_bytes = locations
        .iter()
        .filter(|l| l.kind == FootprintKind::Cache && matches!(l.safety, Some("safe" | "probably")))
        .map(|l| l.bytes)
        .sum();
    let attribution = locations.first().and_then(|l| {
        engine
            .catalog
            .get()
            .and_then(|c| c.attribute(&l.path, None))
    });
    let running = locations
        .iter()
        .filter(|l| l.kind == FootprintKind::Cache)
        .take(8)
        .any(|l| {
            strata_clean::apps::running_app_warnings(std::path::Path::new(&l.path), &[])
                .is_ok_and(|w| !w.is_empty())
        });
    let estimate = app.and_then(|a| a.estimated_size);
    const BIG: u64 = 100 * 1024 * 1024;
    let mismatch = estimate.is_some_and(|e| {
        e > BIG && total_bytes > BIG && (e > total_bytes * 4 || total_bytes > e * 4)
    });
    AppFootprint {
        id,
        name,
        publisher: app.and_then(|a| a.publisher.clone()),
        version: app.and_then(|a| a.version.clone()),
        source: match app.map(|a| &a.source) {
            None => "rule",
            Some(AppSource::Appx) => "appx",
            Some(AppSource::Launcher { .. }) => "launcher",
            Some(AppSource::Other) => "heuristic",
            Some(_) => "registry",
        },
        confidence: attribution
            .as_ref()
            .map_or(if app.is_some() { "high" } else { "heuristic" }, |a| {
                confidence_name(a.confidence)
            }),
        evidence: attribution
            .map(|a| {
                a.evidence
                    .iter()
                    .map(crate::detail::evidence_text)
                    .collect()
            })
            .unwrap_or_default(),
        total_bytes,
        locations,
        registry_estimate_bytes: estimate,
        mismatch,
        can_uninstall: app.is_some_and(|a| a.uninstall_string.is_some()),
        cache_bytes,
        running,
    }
}

/// Leftover app data with no installed owner (`OrphanFolder` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanFolder {
    /// Folder.
    pub path: String,
    /// Volume.
    pub volume_id: String,
    /// Wire id.
    pub entry_id: Option<u32>,
    /// Allocated bytes.
    pub bytes: u64,
    /// Newest modification inside (Unix ms).
    pub last_activity_ms: Option<i64>,
    /// Best guess at the former owner.
    pub guessed_app: Option<String>,
    /// Why it looks orphaned.
    pub reason: String,
}

/// Folders under app-data roots that look like data of an uninstalled app.
#[must_use]
pub fn orphans(state: &AppState) -> Vec<OrphanFolder> {
    let Some(engine) = state.engine.get() else {
        return Vec::new();
    };
    let Some(catalog) = engine.catalog.get() else {
        return Vec::new();
    };
    let now = now_epoch2000();
    let mut out = Vec::new();
    for (volume_id, session) in indexed(state) {
        let g = read(&session.data);
        let Some(data) = g.as_ref() else { continue };
        let ix = &data.index;
        // App-data roots sit at most five levels deep
        // (`C:\Users\<user>\AppData\Local\<Vendor>`).
        let mut stack = vec![(ix.root(), 0u8)];
        while let Some((d, depth)) = stack.pop() {
            for k in ix.children(d) {
                if !ix.is_dir(k) || ix.flags(k).contains(EntryFlags::VIRTUAL) {
                    continue;
                }
                if depth < 5 {
                    stack.push((k, depth + 1));
                }
                if depth < 1 {
                    continue;
                }
                let path = data.path(k);
                let rule_app = data
                    .class(k)
                    .and_then(|c| c.rule)
                    .and_then(|r| engine.classifier.rule(r).app.clone());
                let modified = data.modified(k);
                let idle_days = if modified == 0 {
                    u32::MAX
                } else {
                    now.saturating_sub(modified) / 86_400
                };
                if let Some(o) = catalog.orphan_check(&path, rule_app.as_deref(), idle_days) {
                    out.push(OrphanFolder {
                        path: o.folder,
                        volume_id: volume_id.clone(),
                        entry_id: Some(data.wire(k)),
                        bytes: ix.size(k, SizeMode::Allocated),
                        last_activity_ms: epoch2000_to_ms(modified),
                        guessed_app: None,
                        reason: o.reason,
                    });
                }
            }
        }
    }
    out.sort_by_key(|x| std::cmp::Reverse(x.bytes));
    out
}

// -----------------------------------------------------------------------------
// Recommendations
// -----------------------------------------------------------------------------

/// What acting on a recommendation does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecommendationAction {
    /// Queue its items.
    Queue,
    /// Open a tool.
    Tool {
        /// The tool (`ToolAction` in the UI).
        tool: serde_json::Value,
    },
    /// Open a view.
    View {
        /// `duplicates`, `apps` or `largest`.
        view: &'static str,
    },
}

/// One ranked finding (`Recommendation` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recommendation {
    /// Id for preview and queueing.
    pub id: String,
    /// Kind, e.g. `caches`, `stale_node_modules`.
    pub kind: &'static str,
    /// Title.
    pub title: String,
    /// One-line summary.
    pub summary: String,
    /// Why it is safe or what to check.
    pub explain: String,
    /// Bytes.
    pub bytes: u64,
    /// Items.
    pub items: u64,
    /// Strictest tier among its items.
    pub safety: &'static str,
    /// Action.
    pub action: RecommendationAction,
}

/// One item of a recommendation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecommendationItem {
    /// Volume.
    pub volume_id: String,
    /// Wire id.
    pub entry_id: u32,
    /// Path.
    pub path: String,
    /// Bytes.
    pub bytes: u64,
    /// Tier.
    pub safety: &'static str,
    /// Why.
    pub explain: String,
}

/// Recommendations from the last [`recommendations`] call, with their items.
#[derive(Debug, Default)]
pub struct RecommendationCache(Mutex<HashMap<String, Vec<RecommendationItem>>>);

impl RecommendationCache {
    /// The items of recommendation `id`.
    #[must_use]
    pub fn items(&self, id: &str) -> Option<Vec<RecommendationItem>> {
        crate::state::lock(&self.0).get(id).cloned()
    }
}

/// Thresholds from settings.
#[derive(Debug, Clone, Copy)]
pub struct RecommendationConfig {
    /// `node_modules` untouched this long are stale.
    pub stale_node_modules_days: u32,
    /// Installers in Downloads older than this are stale.
    pub stale_installers_days: u32,
    /// Bytes in the Recycle Bin.
    pub recycle_bin_bytes: u64,
    /// Items in the Recycle Bin.
    pub recycle_bin_items: u64,
    /// Bytes wasted by duplicates from the last duplicate scan.
    pub duplicate_bytes: u64,
    /// Duplicate groups from the last duplicate scan.
    pub duplicate_groups: u64,
}

fn strictest(items: &[RecommendationItem]) -> &'static str {
    let rank = |s: &str| match s {
        "safe" => 0,
        "probably" => 1,
        "careful" => 2,
        _ => 3,
    };
    items
        .iter()
        .map(|i| i.safety)
        .max_by_key(|s| rank(s))
        .unwrap_or("safe")
}

fn gib(b: u64) -> String {
    crate::features::tray::format_bytes(b, strata_store::SizeUnits::Binary)
}

/// Ranks what can be freed: regenerable caches by rule, stale
/// `node_modules`, old installers in Downloads, the Recycle Bin and the
/// Windows Update download cache. Every item is a top-level entry (nothing
/// is counted twice).
#[must_use]
pub fn recommendations(
    state: &AppState,
    cache: &RecommendationCache,
    cfg: &RecommendationConfig,
) -> Vec<Recommendation> {
    let Some(engine) = state.engine.get() else {
        return Vec::new();
    };
    let now = now_epoch2000();
    let mut by_rule: BTreeMap<u32, Vec<RecommendationItem>> = BTreeMap::new();
    let mut node_modules = Vec::new();
    let mut installers = Vec::new();
    let mut update_cache = Vec::new();
    let downloads: Vec<String> = strata_win::known::known_folders()
        .map(|kf| {
            kf.users
                .iter()
                .filter_map(|u| u.folders.get(&strata_core::known::KnownFolder::Downloads))
                .map(|p| path_key(&p.display().to_string()))
                .collect()
        })
        .unwrap_or_default();
    for (volume_id, session) in indexed(state) {
        let g = read(&session.data);
        let Some(data) = g.as_ref() else { continue };
        let ix = &data.index;
        let item = |id: EntryId, explain: String| RecommendationItem {
            volume_id: volume_id.clone(),
            entry_id: data.wire(id),
            path: data.path(id),
            bytes: ix.size(id, SizeMode::Allocated),
            safety: shown_safety(data, id).map_or("careful", safety_name),
            explain,
        };
        let mut stack = vec![ix.root()];
        while let Some(d) = stack.pop() {
            for k in ix.children(d) {
                let f = ix.flags(k);
                if f.contains(EntryFlags::VIRTUAL) {
                    continue;
                }
                let class = data.class(k);
                let parent_rule = data.class(d).and_then(|c| c.rule);
                let rule = class.and_then(|c| c.rule);
                // Regenerable safe/probably data, counted where its rule
                // starts applying.
                if let (Some(r), Some(c)) = (rule, class)
                    && rule != parent_rule
                    && matches!(c.safety, Safety::Safe | Safety::Probably)
                    && engine.classifier.rule(r).regenerable
                    && engine.classifier.rule(r).action == strata_classify::Action::Delete
                {
                    let explain = engine.classifier.rule(r).explain.clone();
                    by_rule.entry(r.0).or_default().push(item(k, explain));
                    continue;
                }
                let is_dir = ix.is_dir(k);
                let name = ix.name_lossy(k);
                if is_dir && name.eq_ignore_ascii_case("node_modules") {
                    let idle = now.saturating_sub(data.modified(k)) / 86_400;
                    if idle >= cfg.stale_node_modules_days {
                        node_modules.push(item(
                            k,
                            format!("Untouched for {idle} days; `npm install` re-creates it."),
                        ));
                    }
                    continue;
                }
                if !is_dir {
                    let ext = ix.extension(k).unwrap_or_default();
                    let in_downloads = downloads
                        .iter()
                        .any(|dl| path_key(&data.path(d)).starts_with(dl.as_str()));
                    if in_downloads
                        && matches!(ext.as_str(), "exe" | "msi" | "msix" | "msixbundle" | "iso")
                    {
                        let idle = now.saturating_sub(data.modified(k)) / 86_400;
                        if idle >= cfg.stale_installers_days {
                            installers.push(item(
                                k,
                                format!("An installer downloaded {idle} days ago; download it again if you need it."),
                            ));
                        }
                    }
                    continue;
                }
                if path_key(&data.path(k)).ends_with(r"\windows\softwaredistribution\download") {
                    update_cache.push(item(k, "Windows Update downloads; Disk Cleanup removes the ones already installed.".into()));
                    continue;
                }
                stack.push(k);
            }
        }
    }
    let mut out = Vec::new();
    let mut items_map = HashMap::new();
    let mut push =
        |rec: Recommendation, items: Vec<RecommendationItem>, out: &mut Vec<Recommendation>| {
            items_map.insert(rec.id.clone(), items);
            out.push(rec);
        };
    for (r, items) in by_rule {
        let bytes: u64 = items.iter().map(|i| i.bytes).sum();
        if bytes < 50 * 1024 * 1024 {
            continue;
        }
        let rule = engine.classifier.rule(strata_classify::RuleId(r));
        push(
            Recommendation {
                id: format!("rule:{}", rule.id),
                kind: "caches",
                title: rule.name.clone(),
                summary: format!(
                    "{} in {} location{}",
                    gib(bytes),
                    items.len(),
                    if items.len() == 1 { "" } else { "s" }
                ),
                explain: rule.explain.clone(),
                bytes,
                items: items.len() as u64,
                safety: strictest(&items),
                action: RecommendationAction::Queue,
            },
            items,
            &mut out,
        );
    }
    for (id, kind, title, items, explain) in [
        (
            "stale_node_modules",
            "stale_node_modules",
            "Old node_modules folders",
            node_modules,
            "Dependency folders of projects you haven't touched in a while. Running `npm install` (or your package manager) re-creates them.",
        ),
        (
            "old_installers",
            "old_installers",
            "Old installers in Downloads",
            installers,
            "Setup files you already ran. Keep any you can't download again.",
        ),
    ] {
        let bytes: u64 = items.iter().map(|i| i.bytes).sum();
        if items.is_empty() {
            continue;
        }
        push(
            Recommendation {
                id: id.into(),
                kind,
                title: title.into(),
                summary: format!(
                    "{} in {} item{}",
                    gib(bytes),
                    items.len(),
                    if items.len() == 1 { "" } else { "s" }
                ),
                explain: explain.into(),
                bytes,
                items: items.len() as u64,
                safety: strictest(&items),
                action: RecommendationAction::Queue,
            },
            items,
            &mut out,
        );
    }
    let update_bytes: u64 = update_cache.iter().map(|i| i.bytes).sum();
    if update_bytes > 256 * 1024 * 1024 {
        push(
            Recommendation {
                id: "windows_update".into(),
                kind: "windows_update",
                title: "Windows Update downloads".into(),
                summary: gib(update_bytes),
                explain: "Windows keeps update packages after installing them. Disk Cleanup removes the ones it no longer needs, safely.".into(),
                bytes: update_bytes,
                items: update_cache.len() as u64,
                safety: "careful",
                action: RecommendationAction::Tool {
                    tool: serde_json::json!({ "kind": "disk_cleanup", "drive": null }),
                },
            },
            update_cache,
            &mut out,
        );
    }
    if cfg.recycle_bin_bytes > 0 {
        push(
            Recommendation {
                id: "recycle_bin".into(),
                kind: "recycle_bin",
                title: "Recycle Bin".into(),
                summary: format!("{} in {} item{}", gib(cfg.recycle_bin_bytes), cfg.recycle_bin_items, if cfg.recycle_bin_items == 1 { "" } else { "s" }),
                explain: "Deleted files still take space until the Recycle Bin is emptied. Emptying it cannot be undone.".into(),
                bytes: cfg.recycle_bin_bytes,
                items: cfg.recycle_bin_items,
                safety: "careful",
                action: RecommendationAction::Tool {
                    tool: serde_json::json!({ "kind": "empty_recycle_bin", "drive": null }),
                },
            },
            Vec::new(),
            &mut out,
        );
    }
    if cfg.duplicate_bytes > 0 {
        push(
            Recommendation {
                id: "duplicates".into(),
                kind: "duplicates",
                title: "Duplicate files".into(),
                summary: format!(
                    "{} in {} group{}",
                    gib(cfg.duplicate_bytes),
                    cfg.duplicate_groups,
                    if cfg.duplicate_groups == 1 { "" } else { "s" }
                ),
                explain: "Identical copies of the same files. Review each group and keep at least one copy; Strata never removes every copy.".into(),
                bytes: cfg.duplicate_bytes,
                items: cfg.duplicate_groups,
                safety: "careful",
                action: RecommendationAction::View { view: "duplicates" },
            },
            Vec::new(),
            &mut out,
        );
    }
    out.sort_by_key(|x| std::cmp::Reverse(x.bytes));
    *crate::state::lock(&cache.0) = items_map;
    out
}

/// Seconds since 2000 for "now" minus `days`.
#[must_use]
pub fn days_ago(days: u32) -> EpochSecs {
    EpochSecs(now_epoch2000().saturating_sub(days.saturating_mul(86_400)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_groups() {
        assert_eq!(extension_group("mkv"), "Video");
        assert_eq!(extension_group("gguf"), "Model or data");
        assert_eq!(extension_group(""), "No extension");
        assert_eq!(extension_group("xyz"), "Other");
    }

    #[test]
    fn strictest_tier_wins() {
        let item = |s| RecommendationItem {
            volume_id: String::new(),
            entry_id: 0,
            path: String::new(),
            bytes: 0,
            safety: s,
            explain: String::new(),
        };
        assert_eq!(strictest(&[item("safe"), item("careful")]), "careful");
        assert_eq!(strictest(&[]), "safe");
    }
}
