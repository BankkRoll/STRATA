//! `entry_info` and `entry_detail` payloads (`ui/src/lib/entries.ts`,
//! `ui/src/lib/detail.ts`).
//!
//! [`entry_info`] answers from the index alone (tooltips and labels call it
//! in batches). [`entry_detail`] adds live facts the index does not keep:
//! full-precision timestamps, named streams, the reparse target and a
//! content sniff, plus the classification explanation, the app attribution
//! with its evidence, and the directory's size history.

use std::path::Path;

use serde::Serialize;
use strata_classify::catalog::{Confidence, Evidence};
use strata_classify::sniff;
use strata_core::{CloudState, EntryFlags, FileTime, ReparseKind, Safety, SizeMode, win32};
use strata_index::EntryId;

use crate::classify::{Engine, safety_code};
use crate::model::{VolumeData, epoch2000_to_ms, now_filetime};
use crate::scan::{SHADOW_NAME, UNACCOUNTED_NAME};
use crate::shell;

/// Safety names indexed by wire code.
const SAFETY: [Option<&str>; 5] = [
    None,
    Some("safe"),
    Some("probably"),
    Some("careful"),
    Some("never"),
];

/// One `entry_info` element.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryInfo {
    /// Wire id.
    pub id: u32,
    /// Display name.
    pub name: String,
    /// Directory.
    pub is_dir: bool,
    /// Allocated (subtree for directories).
    pub allocated: u64,
    /// Logical (subtree for directories).
    pub logical: u64,
    /// Items below.
    pub items: u64,
    /// Category id (dominant for directories).
    pub category: u16,
    /// Owning app label.
    pub app: Option<String>,
    /// Safety tier.
    pub safety: Option<&'static str>,
    /// Modified (Unix ms).
    pub modified_ms: Option<i64>,
    /// Some timestamp is implausible.
    pub suspicious_time: bool,
}

/// Builds the info of one entry.
#[must_use]
pub fn entry_info(data: &VolumeData, engine: Option<&Engine>, id: EntryId) -> EntryInfo {
    let ix = &data.index;
    let app = ix.owner_app(id);
    EntryInfo {
        id: data.wire(id),
        name: ix.name_lossy(id),
        is_dir: ix.is_dir(id),
        allocated: ix.size(id, SizeMode::Allocated),
        logical: ix.size(id, SizeMode::Logical),
        items: data.items(id),
        category: (data.key_static(id) & 0xF) as u16,
        app: engine.and_then(|e| e.apps.name(app)),
        safety: SAFETY[usize::from(safety_code(data.class_bits(id)))],
        modified_ms: epoch2000_to_ms(data.modified(id)),
        suspicious_time: ix.flags(id).contains(EntryFlags::SUSPICIOUS_TIME),
    }
}

/// A timestamp with provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DetailTime {
    /// Unix ms.
    pub ms: Option<i64>,
    /// Implausible (pre-1990 or future).
    pub suspicious: bool,
    /// The full FILETIME (100 ns since 1601) as a decimal string, when it
    /// was read from the filesystem.
    pub filetime: Option<String>,
}

impl DetailTime {
    fn from_filetime(t: u64, now: FileTime) -> Self {
        Self {
            ms: (t != 0).then(|| {
                let ft = FileTime(t);
                ft.to_unix_secs() * 1000 + i64::try_from((t % 10_000_000) / 10_000).unwrap_or(0)
            }),
            suspicious: t != 0 && FileTime(t).is_suspicious(now),
            filetime: (t != 0).then(|| t.to_string()),
        }
    }

    fn from_epoch(secs: u32, now: FileTime) -> Self {
        Self {
            ms: epoch2000_to_ms(secs),
            suspicious: secs != 0
                && strata_core::EpochSecs(secs)
                    .to_filetime()
                    .is_suspicious(now),
            filetime: None,
        }
    }
}

/// Sizes section.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailSizes {
    logical: u64,
    allocated: u64,
    ads_logical: u64,
    ads_allocated: u64,
    dir_overhead: u64,
    compression_ratio: Option<f64>,
    estimated: bool,
}

/// Counts section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DetailCounts {
    files: u32,
    dirs: u32,
}

/// Times section.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailTimes {
    created: DetailTime,
    modified: DetailTime,
    accessed: DetailTime,
    mft_changed: DetailTime,
    file_name_created: Option<DetailTime>,
    access_unreliable: bool,
}

/// Reparse section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DetailReparse {
    kind: &'static str,
    tag: u32,
    target: Option<String>,
}

/// Cloud section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailCloud {
    state: &'static str,
    cloud_logical: u64,
}

/// Hardlinks section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailHardlinks {
    paths: Vec<String>,
    counted_at: String,
}

/// One named stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DetailStream {
    name: String,
    logical: u64,
    allocated: u64,
}

/// Content sniff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailDetected {
    label: String,
    claimed_extension: Option<String>,
    mismatch: bool,
}

/// Classification section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailClass {
    category: u16,
    rule_id: String,
    rule_name: String,
    explain: String,
}

/// Attribution section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DetailAttribution {
    app: String,
    confidence: &'static str,
    evidence: Vec<String>,
}

/// Safety section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DetailSafety {
    tier: &'static str,
    why: String,
    regenerable: bool,
}

/// The process that last wrote an entry (`lastWriter` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastWriter {
    /// Executable name.
    pub process: String,
    /// Process id at the time (0 when unknown).
    pub pid: u32,
    /// When (Unix ms).
    pub at_ms: i64,
}

/// One history point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPoint {
    /// Snapshot time (Unix ms).
    pub at_ms: i64,
    /// Allocated bytes of the directory.
    pub allocated: u64,
}

/// `EntryDetail`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryDetail {
    id: u32,
    volume_id: String,
    name: String,
    path: String,
    is_dir: bool,
    icon_data_url: Option<String>,
    sizes: DetailSizes,
    counts: Option<DetailCounts>,
    times: DetailTimes,
    flags: u32,
    reparse: Option<DetailReparse>,
    cloud: Option<DetailCloud>,
    hardlinks: Option<DetailHardlinks>,
    streams: Vec<DetailStream>,
    detected_type: Option<DetailDetected>,
    classification: Option<DetailClass>,
    attribution: Option<DetailAttribution>,
    safety: Option<DetailSafety>,
    /// Filled by the caller from the store's last-writer table.
    pub last_writer: Option<LastWriter>,
    /// Filled by the caller from the store.
    pub history: Option<Vec<HistoryPoint>>,
    partial: bool,
}

fn reparse_kind_name(k: ReparseKind) -> (&'static str, u32) {
    match k {
        ReparseKind::None => ("unknown", 0),
        ReparseKind::Symlink => ("symlink", win32::IO_REPARSE_TAG_SYMLINK),
        ReparseKind::MountPoint => ("mount_point", win32::IO_REPARSE_TAG_MOUNT_POINT),
        ReparseKind::Wof => ("wof", win32::IO_REPARSE_TAG_WOF),
        ReparseKind::Cloud => ("cloud", 0),
        ReparseKind::Dedup => ("dedup", win32::IO_REPARSE_TAG_DEDUP),
        ReparseKind::AppExecLink => ("app_exec_link", win32::IO_REPARSE_TAG_APPEXECLINK),
        ReparseKind::Wsl => ("wsl", 0),
        ReparseKind::Unknown => ("unknown", 0),
    }
}

/// One line of attribution evidence for the UI.
#[must_use]
pub fn evidence_text(e: &Evidence) -> String {
    match e {
        Evidence::InstallLocation { root } => {
            format!("Inside the registered install folder {root}")
        }
        Evidence::BinaryLocation { root } => {
            format!("Inside the folder of its program or uninstaller, {root}")
        }
        Evidence::PackageInstall { root } => format!("Inside its app package folder {root}"),
        Evidence::PackageData { root } => format!("Inside its app package data folder {root}"),
        Evidence::RuleLabel { rule, label } => format!("Rule {rule} labels this data as {label}"),
        Evidence::Activity { prefix, weight } => {
            format!("Seen writing under {prefix} (weight {weight:.1})")
        }
        Evidence::FolderName {
            folder,
            matched,
            score,
        } => format!(
            "Folder name \"{folder}\" resembles \"{matched}\" ({:.0}%)",
            score * 100.0
        ),
    }
}

/// Builds the full detail of one entry. Reads live filesystem facts; must
/// run off the UI thread.
#[must_use]
pub fn entry_detail(
    data: &VolumeData,
    engine: Option<&Engine>,
    volume_id: &str,
    id: EntryId,
    access_unreliable: bool,
) -> EntryDetail {
    let ix = &data.index;
    let flags = ix.flags(id);
    let is_dir = ix.is_dir(id);
    let path = data.path(id);
    let p = Path::new(&path);
    let is_virtual = flags.contains(EntryFlags::VIRTUAL);
    let now = now_filetime();
    let times = ix.times(id).unwrap_or_default();
    let live = (!is_virtual).then(|| shell::file_times(p)).flatten();
    let time = |k: usize, fallback: u32| match live {
        Some(t) if t[k] != 0 => DetailTime::from_filetime(t[k], now),
        _ => DetailTime::from_epoch(fallback, now),
    };
    let own_l = ix.own_logical(id);
    let own_a = ix.own_allocated(id);
    let streams: Vec<DetailStream> = if flags.contains(EntryFlags::HAS_ADS) && !is_virtual {
        shell::streams(p)
            .into_iter()
            .filter(|(n, _)| n != "WofCompressedData")
            .map(|(name, logical)| DetailStream {
                name,
                logical,
                allocated: 0,
            })
            .collect()
    } else {
        Vec::new()
    };
    let ads_logical: u64 = streams.iter().map(|s| s.logical).sum();
    let logical = ix.size(id, SizeMode::Logical);
    let allocated = ix.size(id, SizeMode::Allocated);
    let reparse = (flags.reparse() != ReparseKind::None).then(|| {
        let (kind, tag) = reparse_kind_name(flags.reparse());
        DetailReparse {
            kind,
            tag,
            target: matches!(
                flags.reparse(),
                ReparseKind::Symlink | ReparseKind::MountPoint
            )
            .then(|| shell::reparse_target(p))
            .flatten(),
        }
    });
    let cloud = match flags.cloud() {
        CloudState::None => None,
        s => Some(DetailCloud {
            state: match s {
                CloudState::OnlineOnly => "online_only",
                CloudState::AlwaysKeep => "always_keep",
                _ => "locally_available",
            },
            cloud_logical: own_l,
        }),
    };
    let hardlinks = ix.file_ref(id).and_then(|fr| {
        let links = ix.links(fr);
        (links.len() > 1).then(|| DetailHardlinks {
            counted_at: ix.path_string(links[0]),
            paths: links.iter().map(|&l| ix.path_string(l)).collect(),
        })
    });
    let may_read = !is_dir
        && !is_virtual
        && flags.cloud() == CloudState::None
        && !flags.contains(EntryFlags::OFFLINE)
        && flags.reparse() == ReparseKind::None;
    // SECURITY: content is only read from plain local files; cloud
    // placeholders and offline files are never opened for data, so viewing
    // details can never trigger a download.
    let detected_type = may_read
        .then(|| shell::read_head_tail(p, sniff::HEAD_LEN, sniff::TAIL_LEN))
        .flatten()
        .and_then(|(h, t)| sniff::sniff_with_tail(&h, (!t.is_empty()).then_some(&t[..])))
        .map(|d| {
            let name = ix.name_lossy(id);
            let mm = sniff::mismatch(&name, d);
            DetailDetected {
                label: d.label().to_owned(),
                claimed_extension: mm.as_ref().and_then(|m| m.claimed.clone()),
                mismatch: mm.is_some(),
            }
        });
    let class = data.class(id);
    let rule = class.and_then(|c| Some(engine?.classifier.rule(c.rule?)));
    let name = ix.name_lossy(id);
    let virtual_explain = match name.as_str() {
        UNACCOUNTED_NAME if is_virtual => Some(
            "Used space the scan could not attribute to any file: NTFS metadata ($MFT, \
             $LogFile, USN journal), shadow copies that could not be queried, folders the \
             standard scan could not read, and allocation rounding.",
        ),
        SHADOW_NAME if is_virtual => Some(
            "Space used by Volume Shadow Copies (System Restore points). Manage it in \
             System Protection settings.",
        ),
        _ => None,
    };
    let classification = match (class, rule) {
        (Some(c), Some(r)) => Some(DetailClass {
            category: c.category as u16,
            rule_id: r.id.clone(),
            rule_name: r.name.clone(),
            explain: r.explain.clone(),
        }),
        (Some(c), None) if virtual_explain.is_some() => Some(DetailClass {
            category: c.category as u16,
            rule_id: "builtin.virtual_block".into(),
            rule_name: name.clone(),
            explain: virtual_explain.unwrap_or_default().to_owned(),
        }),
        _ => None,
    };
    let safety = class.and_then(|c| {
        let tier = SAFETY[usize::from(safety_code(data.class_bits(id)))]?;
        Some(DetailSafety {
            tier,
            why: rule.map_or_else(
                || match c.safety {
                    Safety::Never => "Windows, program and NTFS system data is protected: Strata never offers to delete it.".to_owned(),
                    _ => "No rule covers this item.".to_owned(),
                },
                |r| r.explain.clone(),
            ),
            regenerable: rule.is_some_and(|r| r.regenerable),
        })
    });
    let attribution = engine
        .zip(class)
        .filter(|_| !is_virtual)
        .and_then(|(e, c)| e.attribute(&path, &c))
        .map(|a| DetailAttribution {
            app: a.label,
            confidence: match a.confidence {
                Confidence::Exact => "exact",
                Confidence::High => "high",
                Confidence::Heuristic => "heuristic",
            },
            evidence: a.evidence.iter().map(evidence_text).collect(),
        });
    EntryDetail {
        id: data.wire(id),
        volume_id: volume_id.to_owned(),
        name,
        path,
        is_dir,
        icon_data_url: None,
        sizes: DetailSizes {
            logical,
            allocated,
            ads_logical,
            ads_allocated: 0,
            dir_overhead: if is_dir { own_a } else { 0 },
            compression_ratio: (logical > 0).then(|| allocated as f64 / logical as f64),
            estimated: flags.contains(EntryFlags::ALLOC_ESTIMATED),
        },
        counts: ix.aggregate(id).map(|a| DetailCounts {
            files: a.files,
            dirs: a.dirs,
        }),
        times: DetailTimes {
            created: time(0, times.created.0),
            modified: time(1, times.modified.0),
            accessed: time(2, times.accessed.0),
            mft_changed: DetailTime::from_epoch(times.changed.0, now),
            file_name_created: None,
            access_unreliable,
        },
        flags: flags.0,
        reparse,
        cloud,
        hardlinks,
        streams,
        detected_type,
        classification,
        attribution,
        safety,
        last_writer: None,
        history: None,
        partial: flags.contains(EntryFlags::PARTIAL),
    }
}
