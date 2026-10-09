//! Which copy to keep: a pure, deterministic ranking.
//!
//! Every file gets a key, compared in this order (smaller wins):
//!
//! 1. User rules: inside a "keep" folder, no rule, inside an "avoid" folder
//!    (the most specific matching folder decides).
//! 2. Not inside a temp or cache folder.
//! 3. Not inside a Downloads folder.
//! 4. Oldest last-write time (unknown times rank last).
//! 5. Shortest path.
//! 6. Path, case-insensitively (a tie-break, so the result is stable).
//!
//! The reported reason is the first criterion on which the winner beats
//! the runner-up, i.e. the one that actually decided.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use strata_core::FileTime;

/// Whether a user rule prefers or avoids keeping copies in a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preference {
    /// Keep the copy in this folder.
    Keep,
    /// Prefer deleting the copy in this folder.
    Avoid,
}

/// A user rule: "always keep copies under D:\Photos".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeepRule {
    /// Folder the rule applies to (and everything below it).
    pub folder: PathBuf,
    /// Keep or avoid.
    pub preference: Preference,
}

/// Locations the ranking needs, resolved by the app.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeepContext {
    /// Every Downloads known folder (one per profile the app can see),
    /// resolved through the known-folder API so localized and redirected
    /// folders work.
    pub downloads: Vec<PathBuf>,
    /// Temp folders (`%TEMP%`, `C:\Windows\Temp`, ...). Folders named like
    /// caches are recognized by name as well.
    pub temp_dirs: Vec<PathBuf>,
    /// User rules.
    pub rules: Vec<KeepRule>,
}

/// Why a copy was suggested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeepReason {
    /// A user rule decided.
    UserRule {
        /// The rule's folder.
        folder: PathBuf,
        /// Whether it was a keep rule on the winner or an avoid rule on the
        /// others.
        preference: Preference,
    },
    /// The others are in temp or cache folders.
    NotInTempOrCache,
    /// The others are in Downloads.
    NotInDownloads,
    /// The oldest copy.
    Oldest,
    /// The shortest path.
    ShortestPath,
    /// Nothing distinguishes the copies; the first by path was chosen.
    FirstByPath,
}

impl KeepReason {
    /// One-line explanation for the UI.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::UserRule {
                folder,
                preference: Preference::Keep,
            } => format!("Your rule keeps copies in {}", folder.display()),
            Self::UserRule {
                folder,
                preference: Preference::Avoid,
            } => format!("Your rule prefers removing copies in {}", folder.display()),
            Self::NotInTempOrCache => "The other copies are in temp or cache folders".into(),
            Self::NotInDownloads => "The other copies are in Downloads".into(),
            Self::Oldest => "Oldest copy".into(),
            Self::ShortestPath => "Shortest path".into(),
            Self::FirstByPath => "Copies are equivalent; first by path".into(),
        }
    }
}

/// The suggested copy to keep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepSuggestion {
    /// Index into the group's files.
    pub index: usize,
    /// Why.
    pub reason: KeepReason,
}

/// One copy as the ranking sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepInput<'a> {
    /// Full path.
    pub path: &'a Path,
    /// Last-write time (0 = unknown).
    pub mtime: FileTime,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    rule: u8,
    temp: bool,
    downloads: bool,
    mtime: u64,
    len: usize,
    folded: String,
}

/// Picks the copy to keep. `files` must not be empty.
///
/// # Panics
///
/// Never for non-empty input; an empty slice returns index 0 with
/// [`KeepReason::FirstByPath`] rather than panicking.
///
/// # Example
///
/// ```
/// use std::path::Path;
/// use strata_core::FileTime;
/// use strata_dupes::{KeepContext, KeepInput, KeepReason, suggest_keep};
/// let ctx = KeepContext { downloads: vec![r"C:\Users\me\Downloads".into()], ..Default::default() };
/// let files = [
///     KeepInput { path: Path::new(r"C:\Users\me\Downloads\a.iso"), mtime: FileTime(1) },
///     KeepInput { path: Path::new(r"D:\iso\a.iso"), mtime: FileTime(2) },
/// ];
/// let s = suggest_keep(&files, &ctx);
/// assert_eq!((s.index, s.reason), (1, KeepReason::NotInDownloads));
/// ```
#[must_use]
pub fn suggest_keep(files: &[KeepInput<'_>], ctx: &KeepContext) -> KeepSuggestion {
    let ranked: Vec<(Rank, Option<&KeepRule>)> = files.iter().map(|f| rank(f, ctx)).collect();
    let Some(best) = (0..ranked.len()).min_by(|&a, &b| ranked[a].0.cmp(&ranked[b].0)) else {
        return KeepSuggestion {
            index: 0,
            reason: KeepReason::FirstByPath,
        };
    };
    let runner_up = (0..ranked.len())
        .filter(|&i| i != best)
        .min_by(|&a, &b| ranked[a].0.cmp(&ranked[b].0));
    let reason = match runner_up {
        None => KeepReason::FirstByPath,
        Some(r) => {
            let (w, l) = (&ranked[best], &ranked[r]);
            if w.0.rule != l.0.rule {
                // The deciding rule is the winner's keep rule or, failing
                // that, the runner-up's avoid rule.
                let rule = w.1.filter(|r| r.preference == Preference::Keep).or(l.1);
                rule.map_or(KeepReason::FirstByPath, |r| KeepReason::UserRule {
                    folder: r.folder.clone(),
                    preference: r.preference,
                })
            } else if w.0.temp != l.0.temp {
                KeepReason::NotInTempOrCache
            } else if w.0.downloads != l.0.downloads {
                KeepReason::NotInDownloads
            } else if w.0.mtime != l.0.mtime {
                KeepReason::Oldest
            } else if w.0.len != l.0.len {
                KeepReason::ShortestPath
            } else {
                KeepReason::FirstByPath
            }
        }
    };
    KeepSuggestion {
        index: best,
        reason,
    }
}

fn rank<'c>(f: &KeepInput<'_>, ctx: &'c KeepContext) -> (Rank, Option<&'c KeepRule>) {
    let folded = fold(f.path);
    let rule = ctx
        .rules
        .iter()
        .filter(|r| is_under(&folded, &fold(&r.folder)))
        .max_by_key(|r| fold(&r.folder).len());
    let rule_rank = match rule.map(|r| r.preference) {
        Some(Preference::Keep) => 0,
        None => 1,
        Some(Preference::Avoid) => 2,
    };
    let temp = ctx.temp_dirs.iter().any(|d| is_under(&folded, &fold(d))) || cache_like(&folded);
    let downloads = ctx.downloads.iter().any(|d| is_under(&folded, &fold(d)));
    let mtime = if f.mtime.0 == 0 { u64::MAX } else { f.mtime.0 };
    let rank = Rank {
        rule: rule_rank,
        temp,
        downloads,
        mtime,
        len: folded.chars().count(),
        folded,
    };
    (rank, rule)
}

/// Lowercase, backslash-separated, without a trailing separator or a
/// `\\?\` prefix.
fn fold(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.trim_end_matches('\\').to_lowercase()
}

fn is_under(path: &str, dir: &str) -> bool {
    !dir.is_empty()
        && path.len() > dir.len()
        && path.starts_with(dir)
        && path.as_bytes()[dir.len()] == b'\\'
}

/// Folder names that hold regenerable copies. App cache folder names are
/// not localized, so matching by name is reliable for these.
fn cache_like(folded: &str) -> bool {
    let mut parts: Vec<&str> = folded.split('\\').collect();
    parts.pop();
    parts
        .iter()
        .any(|c| matches!(*c, "temp" | "tmp") || c.contains("cache"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> KeepContext {
        KeepContext {
            downloads: vec![r"C:\Users\me\Downloads".into()],
            temp_dirs: vec![r"C:\Users\me\AppData\Local\Temp".into()],
            rules: vec![],
        }
    }

    fn run(files: &[(&str, u64)], ctx: &KeepContext) -> KeepSuggestion {
        let inputs: Vec<KeepInput<'_>> = files
            .iter()
            .map(|(p, t)| KeepInput {
                path: Path::new(p),
                mtime: FileTime(*t),
            })
            .collect();
        suggest_keep(&inputs, ctx)
    }

    #[test]
    fn table() {
        let keep_rule = KeepContext {
            rules: vec![KeepRule {
                folder: r"D:\Photos".into(),
                preference: Preference::Keep,
            }],
            ..ctx()
        };
        let avoid_rule = KeepContext {
            rules: vec![KeepRule {
                folder: r"E:\Backup".into(),
                preference: Preference::Avoid,
            }],
            ..ctx()
        };
        let nested = KeepContext {
            rules: vec![
                KeepRule {
                    folder: r"D:\Photos".into(),
                    preference: Preference::Keep,
                },
                KeepRule {
                    folder: r"D:\Photos\Dump".into(),
                    preference: Preference::Avoid,
                },
            ],
            ..ctx()
        };
        #[allow(clippy::type_complexity)]
        let cases: &[(&[(&str, u64)], &KeepContext, usize, KeepReason)] = &[
            (
                &[
                    (r"C:\Users\me\Downloads\x.zip", 1),
                    (r"D:\Archive\x.zip", 9),
                ],
                &ctx(),
                1,
                KeepReason::NotInDownloads,
            ),
            (
                &[
                    (r"C:\Users\me\AppData\Local\Temp\x.zip", 1),
                    (r"C:\Users\me\Downloads\x.zip", 9),
                ],
                &ctx(),
                1,
                KeepReason::NotInTempOrCache,
            ),
            (
                &[(r"C:\App\Code Cache\blob", 1), (r"C:\Data\blob", 9)],
                &ctx(),
                1,
                KeepReason::NotInTempOrCache,
            ),
            (
                &[(r"D:\b\x.iso", 20), (r"D:\a\x.iso", 10)],
                &ctx(),
                1,
                KeepReason::Oldest,
            ),
            (
                &[(r"D:\long\path\x.iso", 10), (r"D:\x.iso", 10)],
                &ctx(),
                1,
                KeepReason::ShortestPath,
            ),
            (
                &[(r"D:\b.iso", 10), (r"D:\a.iso", 10)],
                &ctx(),
                1,
                KeepReason::FirstByPath,
            ),
            (
                &[
                    (r"C:\Users\me\Downloads\p.jpg", 1),
                    (r"D:\Photos\2024\p.jpg", 9),
                ],
                &keep_rule,
                1,
                KeepReason::UserRule {
                    folder: r"D:\Photos".into(),
                    preference: Preference::Keep,
                },
            ),
            (
                &[(r"E:\Backup\p.jpg", 1), (r"C:\Users\me\Downloads\p.jpg", 9)],
                &avoid_rule,
                1,
                KeepReason::UserRule {
                    folder: r"E:\Backup".into(),
                    preference: Preference::Avoid,
                },
            ),
            (
                &[(r"D:\Photos\Dump\p.jpg", 1), (r"C:\Other\p.jpg", 9)],
                &nested,
                1,
                KeepReason::UserRule {
                    folder: r"D:\Photos\Dump".into(),
                    preference: Preference::Avoid,
                },
            ),
            (
                // Unknown time ranks last, so the dated copy is "oldest".
                &[(r"D:\a.iso", 0), (r"D:\b.iso", 50)],
                &ctx(),
                1,
                KeepReason::Oldest,
            ),
            (
                // Case and prefix spelling do not matter for folders.
                &[(r"\\?\c:\users\ME\downloads\x.zip", 1), (r"D:\x.zip", 9)],
                &ctx(),
                1,
                KeepReason::NotInDownloads,
            ),
            (
                // A sibling with a common prefix is not "inside".
                &[(r"C:\Users\me\Downloads2\x.zip", 1), (r"D:\x.zip", 9)],
                &ctx(),
                0,
                KeepReason::Oldest,
            ),
            (&[(r"D:\only.iso", 1)], &ctx(), 0, KeepReason::FirstByPath),
        ];
        for (i, (files, c, index, reason)) in cases.iter().enumerate() {
            let s = run(files, c);
            assert_eq!((s.index, &s.reason), (*index, reason), "case {i}");
            assert!(!s.reason.message().is_empty());
        }
    }

    #[test]
    fn empty_input_does_not_panic() {
        assert_eq!(suggest_keep(&[], &ctx()).index, 0);
    }

    #[test]
    fn order_of_input_does_not_change_the_choice() {
        let a = [(r"D:\x\a.bin", 5), (r"D:\y\a.bin", 5), (r"D:\z.bin", 7)];
        let b = [a[2], a[0], a[1]];
        let sa = run(&a, &ctx());
        let sb = run(&b, &ctx());
        assert_eq!(a[sa.index], b[sb.index]);
    }
}
