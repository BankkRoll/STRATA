//! Installed-apps catalog and app attribution ("who owns this").
//!
//! Responsibilities:
//! - Hold [`InstalledApp`] records from the registry Uninstall keys and AppX
//!   packages ([`AppCatalog::load_system`] on Windows; [`AppCatalog::new`]
//!   for any list, which keeps the matching logic pure and testable).
//! - Attribute a path to an app with a [`Confidence`] and an evidence list:
//!   exact install/data locations, rule `app` labels, ETW observations
//!   ([`AppCatalog::add_evidence`]) and folder-name heuristics with fuzzy
//!   matching ([`fuzzy`]).
//! - Detect orphaned app data: vendor/app folders with no installed app.
//!
//! Precedence: an exact location match beats a rule label, which beats ETW
//! evidence, which beats a folder-name guess. Evidence from every source that
//! agrees is kept so the UI can show it.

pub mod fuzzy;
#[cfg(windows)]
mod win;

use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;
use serde::Serialize;
use strata_core::known::{KnownFolder, KnownFolders};

use crate::fold::{normalize_path, path_components, path_components_raw};

/// Where an app record came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AppSource {
    /// `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall`.
    Hklm,
    /// `HKLM\SOFTWARE\WOW6432Node\...\Uninstall` (32-bit apps).
    HklmWow64,
    /// `HKCU\...\Uninstall` (per-user installs).
    Hkcu,
    /// Another loaded user hive under `HKU\<sid>`.
    UserHive {
        /// Account SID.
        sid: String,
    },
    /// AppX/MSIX package.
    Appx,
    /// A game known from a launcher's metadata (see [`crate::discover`]).
    Launcher {
        /// Launcher name, e.g. `Steam`.
        launcher: String,
    },
    /// Supplied by the caller (tests, imports).
    Other,
}

/// One installed application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledApp {
    /// Display name.
    pub name: String,
    /// Publisher.
    pub publisher: Option<String>,
    /// Version string.
    pub version: Option<String>,
    /// Install directory.
    pub install_location: Option<PathBuf>,
    /// `DisplayIcon` value (path, possibly with `,index`).
    pub display_icon: Option<String>,
    /// Registry-estimated size in bytes (`EstimatedSize` is in KiB).
    pub estimated_size: Option<u64>,
    /// Command that uninstalls the app.
    pub uninstall_string: Option<String>,
    /// `InstallDate` (`YYYYMMDD`).
    pub install_date: Option<String>,
    /// Source.
    pub source: AppSource,
    /// Registry key name or AppX package full name.
    pub key: String,
    /// AppX package family name.
    pub package_family: Option<String>,
    /// Known data directories (AppX `%LOCALAPPDATA%\Packages\<family>`).
    pub data_dirs: Vec<PathBuf>,
    /// Hidden from "Apps & features" (`SystemComponent = 1`, frameworks,
    /// system-signed packages).
    pub system_component: bool,
}

impl InstalledApp {
    /// A minimal record (for tests and imports).
    #[must_use]
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            publisher: None,
            version: None,
            install_location: None,
            display_icon: None,
            estimated_size: None,
            uninstall_string: None,
            install_date: None,
            source: AppSource::Other,
            key: name.to_string(),
            package_family: None,
            data_dirs: Vec::new(),
            system_component: false,
        }
    }

    /// A record for a game found in a launcher's metadata, so its folder is
    /// attributed to the game rather than to the launcher.
    #[must_use]
    pub fn from_game(game: &crate::discover::GameInstall) -> Self {
        Self {
            install_location: Some(game.path.clone()),
            estimated_size: game.size,
            source: AppSource::Launcher {
                launcher: game.launcher.to_string(),
            },
            key: format!("{}:{}", game.launcher, game.name),
            ..Self::named(&game.name)
        }
    }
}

/// How sure an attribution is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Name similarity only.
    Heuristic,
    /// A curated rule label, strong ETW evidence, an exact folder-name match,
    /// or an icon/uninstaller location.
    High,
    /// The path is inside the app's registered install or data location.
    Exact,
}

/// One piece of evidence for an attribution.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Evidence {
    /// Inside the registered `InstallLocation`.
    InstallLocation {
        /// The registered root.
        root: String,
    },
    /// Inside the folder holding the app's `DisplayIcon` or uninstaller.
    BinaryLocation {
        /// The folder.
        root: String,
    },
    /// Inside the AppX package's install folder.
    PackageInstall {
        /// The folder.
        root: String,
    },
    /// Inside the AppX package's data folder.
    PackageData {
        /// The folder.
        root: String,
    },
    /// A classifier rule labels this data.
    RuleLabel {
        /// Rule id.
        rule: String,
        /// Label.
        label: String,
    },
    /// ETW saw this app write under the prefix.
    Activity {
        /// Observed prefix.
        prefix: String,
        /// Accumulated weight.
        weight: f32,
    },
    /// A folder name resembles the app or publisher.
    FolderName {
        /// The folder name.
        folder: String,
        /// The name it matched.
        matched: String,
        /// Similarity score.
        score: f32,
    },
}

/// The result of attributing a path to an app.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Attribution {
    /// Display label (app name, rule label or vendor).
    pub label: String,
    /// Matching catalog entries (several when a vendor folder matches many
    /// apps from one publisher).
    pub apps: Vec<usize>,
    /// Confidence.
    pub confidence: Confidence,
    /// Supporting evidence, strongest first.
    pub evidence: Vec<Evidence>,
}

/// A folder that looks like an app's data but matches no installed app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Orphan {
    /// The folder.
    pub folder: String,
    /// Why it is considered orphaned.
    pub reason: String,
}

/// Best fuzzy match of a name against the catalog.
#[derive(Debug)]
struct FuzzyHit {
    apps: Vec<usize>,
    matched: String,
    score: f32,
}

/// A folder-name attribution guess.
#[derive(Debug)]
struct Guess {
    apps: Vec<usize>,
    label: String,
    score: f32,
    evidence: Evidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExactKind {
    Install,
    Binary,
    PackageInstall,
    PackageData,
}

/// Folder names under app-data roots that belong to Windows or are shared
/// by many apps; never treated as an app or as orphaned.
const SHARED_FOLDERS: &[&str] = &[
    "microsoft",
    "packages",
    "temp",
    "programs",
    "publishers",
    "connecteddevicesplatform",
    "comms",
    "d3dscache",
    "crashdumps",
    "diagnostics",
    "elevateddiagnostics",
    "history",
    "inethistory",
    "temporary internet files",
    "virtualstore",
    "application data",
    "placeholdertilelogofolder",
    "package cache",
    "toastnotificationmanagercompat",
    "isolatedstorage",
    "identitynexusintegration",
    ".identityservice",
    "microsoft sdks",
    "common files",
    "windowsapps",
    "windows defender",
    "windows nt",
    "windows mail",
    "windows media player",
    "windows photo viewer",
    "windows portable devices",
    "windows security",
    "windows sidebar",
    "internet explorer",
    "modifiablewindowsapps",
    "reference assemblies",
    "msbuild",
    "dotnet",
    "uninstall information",
    "cache",
    "caches",
    "logs",
    "backup",
    "data",
    "default",
    "desktop.ini",
    "regid.1991-06.com.microsoft",
    "ssh",
    "ssl",
    "usoshared",
    "usoprivate",
    "softwaredistribution",
    "packagestaging",
    "start menu",
    "templates",
    "documents",
    "desktop",
    "favorites",
    "fonts",
    "assembly",
    "squirreltemp",
    "xdg.cache",
    "xdg.config",
    "xdg.data",
];

fn is_shared(folder: &str) -> bool {
    let lower = folder.to_lowercase();
    SHARED_FOLDERS.contains(&lower.as_str())
}

/// The installed-apps catalog.
///
/// # Example
///
/// ```
/// use strata_classify::catalog::{AppCatalog, Confidence, InstalledApp};
/// use strata_core::known::{KnownFolder, KnownFolders, UserFolders};
///
/// let mut kf = KnownFolders::default();
/// let mut me = UserFolders { is_current: true, ..Default::default() };
/// me.folders.insert(KnownFolder::LocalAppData, r"C:\Users\me\AppData\Local".into());
/// kf.users.push(me);
///
/// let mut discord = InstalledApp::named("Discord");
/// discord.install_location = Some(r"C:\Users\me\AppData\Local\Discord".into());
/// let cat = AppCatalog::new(vec![discord], &kf);
/// let a = cat.attribute(r"C:\Users\me\AppData\Local\Discord\app-1.0\Discord.exe", None).unwrap();
/// assert_eq!(a.label, "Discord");
/// assert_eq!(a.confidence, Confidence::Exact);
/// ```
#[derive(Debug, Clone)]
pub struct AppCatalog {
    apps: Vec<InstalledApp>,
    exact: FxHashMap<String, Vec<(usize, ExactKind)>>,
    activity: FxHashMap<String, Vec<(String, f32)>>,
    name_tokens: Vec<Vec<String>>,
    publisher_tokens: Vec<Vec<String>>,
    /// Normalized roots under which the first component names an app/vendor,
    /// with how many components to consider (2 for `Program Files\V\A`) and
    /// whether only dot-folders count (the profile root: `~\.ollama`).
    app_roots: Vec<(String, usize, bool)>,
    /// Normalized roots that must never be treated as an app location.
    generic_roots: Vec<String>,
}

fn parent_of_command(cmd: &str) -> Option<PathBuf> {
    let cmd = cmd.trim();
    let path = if let Some(rest) = cmd.strip_prefix('"') {
        rest.split('"').next()?.to_string()
    } else {
        let lower = cmd.to_ascii_lowercase();
        let end = [".exe", ".dll", ".ico", ".bat", ".cmd"]
            .iter()
            .filter_map(|e| lower.find(e).map(|i| i + e.len()))
            .min()?;
        cmd[..end].to_string()
    };
    let path = path.split(',').next()?.trim().to_string();
    if path.is_empty() || !path.contains('\\') {
        return None;
    }
    Path::new(&path).parent().map(Path::to_path_buf)
}

impl AppCatalog {
    /// Builds a catalog from app records and the machine's known folders.
    #[must_use]
    pub fn new(apps: Vec<InstalledApp>, kf: &KnownFolders) -> Self {
        let mut generic_roots: Vec<String> = Vec::new();
        for f in KnownFolder::ALL {
            for (_, p) in kf.resolve_all(f) {
                generic_roots.push(normalize_path(&p.to_string_lossy()));
            }
        }
        let mut app_roots = Vec::new();
        let mut add_root = |f: KnownFolder, depth: usize, suffix: &str, dot_only: bool| {
            for (_, p) in kf.resolve_all(f) {
                let base = normalize_path(&p.to_string_lossy());
                let r = if suffix.is_empty() {
                    base
                } else {
                    format!("{base}\\{suffix}")
                };
                app_roots.push((r, depth, dot_only));
            }
        };
        add_root(KnownFolder::ProgramFiles, 2, "", false);
        add_root(KnownFolder::ProgramFilesX86, 2, "", false);
        add_root(KnownFolder::LocalAppData, 1, "PROGRAMS", false);
        add_root(KnownFolder::LocalAppData, 2, "", false);
        add_root(KnownFolder::AppData, 2, "", false);
        add_root(KnownFolder::ProgramData, 2, "", false);
        add_root(KnownFolder::UserProfile, 1, "", true);
        for (_, p) in kf.resolve_all(KnownFolder::LocalAppData) {
            let base = normalize_path(&p.to_string_lossy());
            generic_roots.push(format!("{base}\\PROGRAMS"));
            generic_roots.push(format!("{base}\\PACKAGES"));
        }
        for (_, p) in kf.resolve_all(KnownFolder::ProgramFiles) {
            generic_roots.push(format!(
                "{}\\COMMON FILES",
                normalize_path(&p.to_string_lossy())
            ));
            generic_roots.push(format!(
                "{}\\WINDOWSAPPS",
                normalize_path(&p.to_string_lossy())
            ));
        }
        // Longest roots first so `LocalAppData\Programs` wins over `LocalAppData`.
        app_roots.sort_by_key(|(r, _, _)| std::cmp::Reverse(r.len()));

        let mut cat = Self {
            name_tokens: apps.iter().map(|a| fuzzy::tokens(&a.name)).collect(),
            publisher_tokens: apps
                .iter()
                .map(|a| {
                    a.publisher
                        .as_deref()
                        .map(fuzzy::tokens)
                        .unwrap_or_default()
                })
                .collect(),
            apps,
            exact: FxHashMap::default(),
            activity: FxHashMap::default(),
            app_roots,
            generic_roots,
        };
        for i in 0..cat.apps.len() {
            let a = &cat.apps[i];
            let mut locs: Vec<(PathBuf, ExactKind)> = Vec::new();
            if let Some(p) = &a.install_location {
                let kind = if a.source == AppSource::Appx {
                    ExactKind::PackageInstall
                } else {
                    ExactKind::Install
                };
                locs.push((p.clone(), kind));
            }
            for d in &a.data_dirs {
                locs.push((d.clone(), ExactKind::PackageData));
            }
            for cmd in [&a.display_icon, &a.uninstall_string].into_iter().flatten() {
                if let Some(dir) = parent_of_command(cmd) {
                    locs.push((dir, ExactKind::Binary));
                }
            }
            for (p, kind) in locs {
                let n = normalize_path(&p.to_string_lossy());
                if cat.is_generic(&n) {
                    continue;
                }
                let v = cat.exact.entry(n).or_default();
                if !v.iter().any(|(j, k)| *j == i && *k == kind) {
                    v.push((i, kind));
                }
            }
        }
        cat
    }

    /// Reads the registry Uninstall keys and AppX packages of this machine.
    ///
    /// Returns the catalog and non-fatal problems (unreadable keys, AppX
    /// unavailable).
    #[cfg(windows)]
    #[must_use]
    pub fn load_system(kf: &KnownFolders) -> (Self, Vec<String>) {
        let (apps, warnings) = Self::read_system(kf);
        (Self::new(apps, kf), warnings)
    }

    /// Reads the raw app records [`AppCatalog::load_system`] uses, so callers
    /// can add more (e.g. [`InstalledApp::from_game`]) before building.
    ///
    /// Takes a second or two (registry plus AppX enumeration); call it off
    /// the UI thread.
    #[cfg(windows)]
    #[must_use]
    pub fn read_system(kf: &KnownFolders) -> (Vec<InstalledApp>, Vec<String>) {
        let mut warnings = Vec::new();
        let mut apps = win::read_uninstall_entries(&mut warnings);
        let local = kf
            .current_user()
            .and_then(|u| u.folders.get(&KnownFolder::LocalAppData))
            .cloned();
        match win::read_appx_packages(local.as_deref()) {
            Ok(mut a) => apps.append(&mut a),
            Err(e) => warnings.push(format!("AppX packages unavailable: {e}")),
        }
        (apps, warnings)
    }

    /// Every app record.
    #[must_use]
    pub fn apps(&self) -> &[InstalledApp] {
        &self.apps
    }

    fn is_generic(&self, norm: &str) -> bool {
        let comps = path_components(norm);
        comps.len() <= 1
            || self.generic_roots.iter().any(|g| g == norm)
            || comps
                .last()
                .is_some_and(|l| is_shared(l) && comps.len() <= 3)
    }

    /// Records live-activity evidence (from ETW): `app` wrote under
    /// `path_prefix`. Weights accumulate; a total of 1.0 or more counts as
    /// high confidence.
    pub fn add_evidence(&mut self, path_prefix: &str, app: &str, weight: f32) {
        let key = normalize_path(path_prefix);
        let v = self.activity.entry(key).or_default();
        if let Some(e) = v.iter_mut().find(|(a, _)| a == app) {
            e.1 += weight;
        } else {
            v.push((app.to_string(), weight));
        }
    }

    /// Ancestors of a normalized path, longest first, excluding the volume
    /// root (`C:\A\B` yields `C:\A\B`, `C:\A`).
    fn ancestors(norm: &str) -> Vec<&str> {
        let root_len = if norm.starts_with(r"\\") {
            norm.match_indices('\\')
                .nth(3)
                .map_or(norm.len(), |(i, _)| i)
        } else {
            norm.find('\\').unwrap_or(norm.len())
        };
        let mut out = Vec::new();
        let mut end = norm.trim_end_matches('\\').len();
        while end > root_len {
            out.push(&norm[..end]);
            end = norm[..end].rfind('\\').unwrap_or(0);
        }
        out
    }

    fn fuzzy_best(&self, folder: &str, by_publisher: bool) -> Option<FuzzyHit> {
        let t = fuzzy::tokens(folder);
        if t.is_empty() {
            return None;
        }
        let mut best = FuzzyHit {
            apps: Vec::new(),
            matched: String::new(),
            score: 0.0,
        };
        for i in 0..self.apps.len() {
            let mut consider = |tokens: &[String], label: &str| {
                let s = fuzzy::score(&t, tokens);
                if s > best.score + f32::EPSILON {
                    best = FuzzyHit {
                        apps: vec![i],
                        matched: label.to_string(),
                        score: s,
                    };
                } else if (s - best.score).abs() <= f32::EPSILON
                    && s > 0.0
                    && !best.apps.contains(&i)
                {
                    best.apps.push(i);
                }
            };
            consider(&self.name_tokens[i], &self.apps[i].name);
            if by_publisher && let Some(p) = &self.apps[i].publisher {
                consider(&self.publisher_tokens[i], p);
            }
        }
        (best.score >= 0.75).then_some(best)
    }

    /// Attributes a path to an app.
    ///
    /// `rule_label` is the winning classifier rule's `(id, app)` when it has
    /// an `app` label.
    #[must_use]
    pub fn attribute(&self, path: &str, rule_label: Option<(&str, &str)>) -> Option<Attribution> {
        let norm = normalize_path(path);
        let mut evidence = Vec::new();
        let mut exact: Option<(Vec<usize>, Confidence)> = None;
        for anc in Self::ancestors(&norm) {
            if let Some(hits) = self.exact.get(anc) {
                let best = hits
                    .iter()
                    .map(|(_, k)| *k)
                    .min_by_key(|k| matches!(k, ExactKind::Binary))
                    .unwrap_or(ExactKind::Binary);
                let apps: Vec<usize> = hits
                    .iter()
                    .filter(|(_, k)| *k == best)
                    .map(|(i, _)| *i)
                    .collect();
                let root = anc.to_string();
                evidence.push(match best {
                    ExactKind::Install => Evidence::InstallLocation { root },
                    ExactKind::Binary => Evidence::BinaryLocation { root },
                    ExactKind::PackageInstall => Evidence::PackageInstall { root },
                    ExactKind::PackageData => Evidence::PackageData { root },
                });
                let conf = if best == ExactKind::Binary {
                    Confidence::High
                } else {
                    Confidence::Exact
                };
                exact = Some((apps, conf));
                break;
            }
        }
        if let Some((rule, label)) = rule_label {
            evidence.push(Evidence::RuleLabel {
                rule: rule.to_string(),
                label: label.to_string(),
            });
        }
        let mut activity: Option<(String, f32)> = None;
        for anc in Self::ancestors(&norm) {
            if let Some(v) = self.activity.get(anc) {
                if let Some((app, w)) = v.iter().max_by(|a, b| a.1.total_cmp(&b.1)) {
                    evidence.push(Evidence::Activity {
                        prefix: anc.to_string(),
                        weight: *w,
                    });
                    activity = Some((app.clone(), *w));
                }
                break;
            }
        }

        if let Some((apps, conf)) = exact {
            let label = apps
                .first()
                .map(|&i| self.apps[i].name.clone())
                .unwrap_or_default();
            return Some(Attribution {
                label,
                apps,
                confidence: conf,
                evidence,
            });
        }
        if let Some((_, label)) = rule_label {
            let apps = self
                .fuzzy_best(label, false)
                .map(|b| b.apps)
                .unwrap_or_default();
            return Some(Attribution {
                label: label.to_string(),
                apps,
                confidence: Confidence::High,
                evidence,
            });
        }
        let guess = self.folder_guess(&norm, path);
        if let Some((app, w)) = activity {
            let apps = self
                .fuzzy_best(&app, false)
                .map(|b| b.apps)
                .unwrap_or_default();
            evidence.extend(guess.map(|g| g.evidence));
            return Some(Attribution {
                label: app,
                apps,
                confidence: if w >= 1.0 {
                    Confidence::High
                } else {
                    Confidence::Heuristic
                },
                evidence,
            });
        }
        let g = guess?;
        evidence.push(g.evidence);
        Some(Attribution {
            label: g.label,
            apps: g.apps,
            // Only an identical normalized name is more than a guess.
            confidence: if g.score >= 0.999 {
                Confidence::High
            } else {
                Confidence::Heuristic
            },
            evidence,
        })
    }

    /// Folder components at app/vendor positions below known roots, deepest
    /// first: `Program Files\Vendor\App` yields `App`, then `Vendor`.
    fn app_folders(&self, norm: &str, raw_path: &str) -> Vec<String> {
        let raw = path_components_raw(raw_path);
        for (root, depth, dot_only) in &self.app_roots {
            let Some(rest) = norm.strip_prefix(root.as_str()) else {
                continue;
            };
            if !rest.starts_with('\\') {
                continue;
            }
            let root_len = path_components(root).len();
            let mut out: Vec<String> = raw.iter().skip(root_len).take(*depth).cloned().collect();
            if *dot_only && !out.first().is_some_and(|f| f.starts_with('.')) {
                return Vec::new();
            }
            // A shared parent (`Temp`, `Microsoft`) means the inner folder is
            // not at an app position.
            if let Some(cut) = out.iter().position(|f| is_shared(f)) {
                out.truncate(cut);
            }
            out.reverse();
            return out;
        }
        Vec::new()
    }

    fn folder_guess(&self, norm: &str, raw_path: &str) -> Option<Guess> {
        for folder in self.app_folders(norm, raw_path) {
            let trimmed = folder.trim_start_matches('.');
            if let Some(best) = self.fuzzy_best(trimmed, true) {
                let evidence = Evidence::FolderName {
                    folder: folder.clone(),
                    matched: best.matched.clone(),
                    score: best.score,
                };
                // Several apps behind one name means a vendor folder: label
                // it with the matched publisher.
                let label = if best.apps.len() > 1 {
                    best.matched.clone()
                } else {
                    self.apps[best.apps[0]].name.clone()
                };
                return Some(Guess {
                    apps: best.apps,
                    label,
                    score: best.score,
                    evidence,
                });
            }
        }
        None
    }

    /// Checks whether `folder` is app data with no installed app.
    ///
    /// Only first-level folders under app-data roots count (e.g.
    /// `%APPDATA%\SomeApp`, `ProgramData\Vendor`, `Program Files\Vendor`);
    /// shared Windows folders are excluded. `rule_app` is the classifier's
    /// app label for the folder, if any; labelled data is never orphaned.
    /// `idle_days` is the age of the newest modification in the folder's
    /// subtree: anything written in the last [`ORPHAN_MIN_IDLE_DAYS`] days
    /// still has a live owner (often a CLI tool with no uninstall record).
    ///
    /// The result is only a hint; orphaned data is classified `careful`.
    #[must_use]
    pub fn orphan_check(
        &self,
        folder: &str,
        rule_app: Option<&str>,
        idle_days: u32,
    ) -> Option<Orphan> {
        if rule_app.is_some() || idle_days < ORPHAN_MIN_IDLE_DAYS {
            return None;
        }
        let norm = normalize_path(folder);
        // Only the first-level folder under an app-data root (the vendor or
        // app folder itself); deeper folders belong to whatever owns it. The
        // profile root is excluded: its dot-folders are tools, not apps with
        // uninstall records.
        let root = self.app_roots.iter().find(|(root, _, _)| {
            norm.strip_prefix(root.as_str())
                .is_some_and(|rest| rest.starts_with('\\'))
        })?;
        let rest = &norm[root.0.len()..];
        if root.2 || rest.split('\\').filter(|s| !s.is_empty()).count() != 1 {
            return None;
        }
        let name = path_components_raw(folder).last()?.clone();
        if is_shared(&name) || name.starts_with('.') || self.attribute(folder, None).is_some() {
            return None;
        }
        Some(Orphan {
            folder: folder.to_string(),
            reason: format!(
                "`{name}` looks like app data, no installed app matches it, and nothing in it changed for {idle_days} days"
            ),
        })
    }
}

/// Minimum inactivity before app data without an installed app is reported
/// as orphaned.
pub const ORPHAN_MIN_IDLE_DAYS: u32 = 90;

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::known::UserFolders;

    fn kf() -> KnownFolders {
        let mut kf = KnownFolders::default();
        kf.machine
            .insert(KnownFolder::ProgramFiles, r"C:\Program Files".into());
        kf.machine.insert(
            KnownFolder::ProgramFilesX86,
            r"C:\Program Files (x86)".into(),
        );
        kf.machine
            .insert(KnownFolder::ProgramData, r"C:\ProgramData".into());
        let mut u = UserFolders {
            is_current: true,
            ..Default::default()
        };
        u.folders
            .insert(KnownFolder::UserProfile, r"C:\Users\me".into());
        u.folders.insert(
            KnownFolder::LocalAppData,
            r"C:\Users\me\AppData\Local".into(),
        );
        u.folders
            .insert(KnownFolder::AppData, r"C:\Users\me\AppData\Roaming".into());
        kf.users.push(u);
        kf
    }

    fn catalog() -> AppCatalog {
        let mut obs = InstalledApp::named("OBS Studio");
        obs.publisher = Some("OBS Project".into());
        obs.install_location = Some(r"C:\Program Files\obs-studio".into());
        let mut chrome = InstalledApp::named("Google Chrome");
        chrome.publisher = Some("Google LLC".into());
        chrome.display_icon =
            Some(r#""C:\Program Files\Google\Chrome\Application\chrome.exe",0"#.into());
        let mut earth = InstalledApp::named("Google Earth Pro");
        earth.publisher = Some("Google LLC".into());
        let mut bogus = InstalledApp::named("Bogus");
        bogus.install_location = Some(r"C:\Program Files".into());
        let mut teams = InstalledApp::named("Microsoft Teams");
        teams.source = AppSource::Appx;
        teams.install_location =
            Some(r"C:\Program Files\WindowsApps\MSTeams_1.0_x64__8wekyb3d8bbwe".into());
        teams.data_dirs = vec![r"C:\Users\me\AppData\Local\Packages\MSTeams_8wekyb3d8bbwe".into()];
        let mut discord = InstalledApp::named("Discord");
        discord.uninstall_string =
            Some(r"C:\Users\me\AppData\Local\Discord\Update.exe --uninstall".into());
        AppCatalog::new(vec![obs, chrome, earth, bogus, teams, discord], &kf())
    }

    #[test]
    fn exact_locations() {
        let c = catalog();
        let a = c
            .attribute(r"C:\Program Files\obs-studio\bin\64bit\obs64.exe", None)
            .unwrap();
        assert_eq!(
            (a.label.as_str(), a.confidence),
            ("OBS Studio", Confidence::Exact)
        );
        let a = c
            .attribute(
                r"C:\Users\me\AppData\Local\Packages\MSTeams_8wekyb3d8bbwe\LocalCache",
                None,
            )
            .unwrap();
        assert_eq!(
            (a.label.as_str(), a.confidence),
            ("Microsoft Teams", Confidence::Exact)
        );
        assert!(matches!(a.evidence[0], Evidence::PackageData { .. }));
        let a = c
            .attribute(
                r"C:\Program Files\Google\Chrome\Application\120.0\x.dll",
                None,
            )
            .unwrap();
        assert_eq!(
            (a.label.as_str(), a.confidence),
            ("Google Chrome", Confidence::High)
        );
        let a = c
            .attribute(
                r"C:\Users\me\AppData\Local\Discord\app-1.0.9\Discord.exe",
                None,
            )
            .unwrap();
        assert_eq!(a.label, "Discord");
    }

    #[test]
    fn generic_install_locations_are_ignored() {
        let c = catalog();
        let a = c.attribute(r"C:\Program Files\Unrelated\x.exe", None);
        assert!(a.as_ref().is_none_or(|a| a.label != "Bogus"), "{a:?}");
    }

    #[test]
    fn heuristics_and_vendor_folders() {
        let c = catalog();
        // Roaming obs-studio folder: name equal to the app â†’ high.
        let a = c
            .attribute(r"C:\Users\me\AppData\Roaming\obs-studio\basic", None)
            .unwrap();
        assert_eq!(
            (a.label.as_str(), a.confidence),
            ("OBS Studio", Confidence::High)
        );
        // Vendor folder matches the publisher shared by two apps.
        let a = c
            .attribute(r"C:\Users\me\AppData\Local\Google\CrashReports", None)
            .unwrap();
        assert_eq!(a.apps.len(), 2, "{a:?}");
        assert_eq!(a.label, "Google LLC");
    }

    #[test]
    fn rule_labels_and_activity() {
        let mut c = catalog();
        let a = c
            .attribute(
                r"C:\Users\me\.claude\projects",
                Some(("claude.code.projects", "Claude Code")),
            )
            .unwrap();
        assert_eq!(
            (a.label.as_str(), a.confidence),
            ("Claude Code", Confidence::High)
        );
        c.add_evidence(r"C:\Users\me\weird", "tool.exe", 0.6);
        let a = c.attribute(r"C:\Users\me\weird\x", None).unwrap();
        assert_eq!(
            (a.label.as_str(), a.confidence),
            ("tool.exe", Confidence::Heuristic)
        );
        c.add_evidence(r"C:\Users\me\weird", "tool.exe", 0.6);
        let a = c.attribute(r"C:\Users\me\weird\x", None).unwrap();
        assert_eq!(a.confidence, Confidence::High);
    }

    #[test]
    fn orphans() {
        let c = catalog();
        let idle = 400;
        let check = |p: &str, app: Option<&str>, days: u32| c.orphan_check(p, app, days).is_some();
        assert!(check(r"C:\Users\me\AppData\Roaming\OldGoneApp", None, idle));
        assert!(check(r"C:\ProgramData\OldVendor", None, idle));
        // Recently active: something still owns it.
        assert!(!check(r"C:\Users\me\AppData\Roaming\OldGoneApp", None, 3));
        // Installed, nested, shared, labelled, or a personal folder.
        assert!(!check(
            r"C:\Users\me\AppData\Roaming\obs-studio",
            None,
            idle
        ));
        assert!(!check(
            r"C:\Users\me\AppData\Roaming\OldGoneApp\sub",
            None,
            idle
        ));
        assert!(!check(r"C:\Users\me\AppData\Local\Microsoft", None, idle));
        assert!(!check(
            r"C:\Users\me\AppData\Roaming\Labelled",
            Some("X"),
            idle
        ));
        assert!(!check(r"C:\Users\me\Documents", None, idle));
        assert!(!check(r"C:\Users\me\.oldtool", None, idle));
    }

    #[test]
    fn launcher_games_beat_the_launcher() {
        let game = crate::discover::GameInstall {
            launcher: "Steam",
            name: "Rust".into(),
            path: r"C:\Program Files (x86)\Steam\steamapps\common\Rust".into(),
            size: Some(1),
        };
        let mut steam = InstalledApp::named("Steam");
        steam.display_icon = Some(r"C:\Program Files (x86)\Steam\steam.exe".into());
        let c = AppCatalog::new(vec![steam, InstalledApp::from_game(&game)], &kf());
        let a = c
            .attribute(
                r"C:\Program Files (x86)\Steam\steamapps\common\Rust\x.pak",
                None,
            )
            .unwrap();
        assert_eq!(
            (a.label.as_str(), a.confidence),
            ("Rust", Confidence::Exact)
        );
        let a = c
            .attribute(r"C:\Program Files (x86)\Steam\steamapps\shadercache", None)
            .unwrap();
        assert_eq!(a.label, "Steam");
    }

    #[test]
    fn ancestors_iterate_up_to_drive() {
        let v: Vec<_> = AppCatalog::ancestors(r"C:\A\B\C");
        assert_eq!(v, [r"C:\A\B\C", r"C:\A\B", r"C:\A"]);
        let v: Vec<_> = AppCatalog::ancestors(r"C:\");
        assert!(v.is_empty());
    }

    #[test]
    fn command_parsing() {
        assert_eq!(
            parent_of_command(r#""C:\a b\x.exe" --uninstall"#),
            Some(PathBuf::from(r"C:\a b"))
        );
        assert_eq!(
            parent_of_command(r"C:\a\x.ico,0"),
            Some(PathBuf::from(r"C:\a"))
        );
        assert_eq!(parent_of_command(r"MsiExec.exe /X{GUID}"), None);
    }
}
