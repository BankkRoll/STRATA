//! Runtime discovery of rule roots that live in app configuration rather than
//! at fixed paths.
//!
//! Responsibilities:
//! - Pure parsers: Steam KeyValues (`libraryfolders.vdf`,
//!   `appmanifest_*.acf`), Epic Games Launcher manifests (`*.item`), Firefox
//!   `profiles.ini`, OBS `basic.ini`, Ollama model manifests.
//! - [`discover`]: read those files (read-only) for the current user, plus
//!   cache-location environment variables and, on Windows, the WSL `Lxss`
//!   registry key, and produce [`DynamicRoots`] tokens for rule packs.
//!
//! Tokens produced: `STEAM_LIBRARY`, `EPIC_GAME`, `FIREFOX_PROFILE`,
//! `OBS_RECORDINGS`, `WSL_DISTRO`, `HF_HOME`, `OLLAMA_MODELS`, `CARGO_HOME`,
//! `RUSTUP_HOME`, `GOMODCACHE`.

use std::path::{Path, PathBuf};

use serde::Serialize;
use strata_core::known::{KnownFolder, KnownFolders};

use crate::engine::DynamicRoots;

// -----------------------------------------------------------------------------
// Valve KeyValues (VDF/ACF)
// -----------------------------------------------------------------------------

/// A parsed KeyValues node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kv {
    /// A string value.
    Str(String),
    /// A block of key/value pairs (keys keep file order; duplicates allowed).
    Block(Vec<(String, Kv)>),
}

impl Kv {
    /// The first child named `key` (case-insensitive, as Steam treats keys).
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Kv> {
        match self {
            Self::Block(items) => items
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            Self::Str(_) => None,
        }
    }

    /// The string value, if this is a string.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            Self::Block(_) => None,
        }
    }

    /// Children of a block.
    #[must_use]
    pub fn items(&self) -> &[(String, Kv)] {
        match self {
            Self::Block(items) => items,
            Self::Str(_) => &[],
        }
    }
}

fn kv_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(o) => s.push(o),
                            None => break,
                        },
                        o => s.push(o),
                    }
                }
                out.push(format!("\"{s}"));
            }
            '{' | '}' => out.push(c.to_string()),
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {}
            c => {
                let mut s = String::from(c);
                while let Some(&n) = chars.peek() {
                    if n.is_whitespace() || n == '{' || n == '}' || n == '"' {
                        break;
                    }
                    s.push(n);
                    chars.next();
                }
                out.push(format!("\"{s}"));
            }
        }
    }
    out
}

fn kv_block(tokens: &[String], pos: &mut usize, depth: usize) -> Vec<(String, Kv)> {
    let mut items = Vec::new();
    while *pos < tokens.len() {
        let t = &tokens[*pos];
        if t == "}" {
            *pos += 1;
            break;
        }
        let Some(key) = t.strip_prefix('"') else {
            *pos += 1;
            continue;
        };
        *pos += 1;
        match tokens.get(*pos).map(String::as_str) {
            Some("{") => {
                *pos += 1;
                // Bound recursion so hostile files cannot overflow the stack.
                let v = if depth < 64 {
                    kv_block(tokens, pos, depth + 1)
                } else {
                    Vec::new()
                };
                items.push((key.to_string(), Kv::Block(v)));
            }
            Some(v) if v.starts_with('"') => {
                items.push((key.to_string(), Kv::Str(v[1..].to_string())));
                *pos += 1;
            }
            _ => {}
        }
    }
    items
}

/// Parses Valve KeyValues text (`.vdf`, `.acf`). Malformed input yields a
/// best-effort tree, never a panic.
///
/// # Example
///
/// ```
/// use strata_classify::discover::parse_kv;
/// let kv = parse_kv(r#""AppState" { "name" "Rust" "installdir" "Rust" }"#);
/// assert_eq!(kv.get("AppState").unwrap().get("name").unwrap().as_str(), Some("Rust"));
/// ```
#[must_use]
pub fn parse_kv(text: &str) -> Kv {
    let tokens = kv_tokens(text);
    let mut pos = 0;
    Kv::Block(kv_block(&tokens, &mut pos, 0))
}

/// Steam library folders listed in `steamapps\libraryfolders.vdf`.
#[must_use]
pub fn steam_libraries(vdf: &str) -> Vec<PathBuf> {
    let kv = parse_kv(vdf);
    let Some(root) = kv.get("libraryfolders") else {
        return Vec::new();
    };
    root.items()
        .iter()
        .filter_map(|(_, v)| match v {
            // Current format: numbered blocks with a "path" key.
            Kv::Block(_) => v.get("path").and_then(Kv::as_str).map(PathBuf::from),
            // Pre-2021 format: numbered string values are paths.
            Kv::Str(s) if s.contains('\\') || s.contains(':') => Some(PathBuf::from(s)),
            Kv::Str(_) => None,
        })
        .collect()
}

/// One installed Steam game, from an `appmanifest_*.acf`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SteamApp {
    /// Steam app id.
    pub appid: String,
    /// Game name.
    pub name: String,
    /// Folder name under `steamapps\common`.
    pub installdir: String,
    /// Size on disk reported by Steam.
    pub size_on_disk: Option<u64>,
}

/// Parses an `appmanifest_*.acf`.
#[must_use]
pub fn steam_app_manifest(acf: &str) -> Option<SteamApp> {
    let kv = parse_kv(acf);
    let s = kv.get("AppState")?;
    let field = |k: &str| s.get(k).and_then(Kv::as_str).map(str::to_string);
    Some(SteamApp {
        appid: field("appid")?,
        name: field("name")?,
        installdir: field("installdir")?,
        size_on_disk: field("SizeOnDisk").and_then(|v| v.parse().ok()),
    })
}

// -----------------------------------------------------------------------------
// Epic, Firefox, OBS, Ollama
// -----------------------------------------------------------------------------

/// One Epic Games Launcher install, from a `*.item` manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EpicGame {
    /// Display name.
    pub name: String,
    /// Install folder.
    pub install_location: PathBuf,
    /// Install size in bytes.
    pub size: Option<u64>,
}

/// Parses an Epic Games Launcher manifest (`ProgramData\Epic\EpicGamesLauncher\
/// Data\Manifests\*.item`, JSON).
#[must_use]
pub fn epic_manifest(json: &str) -> Option<EpicGame> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let loc = v.get("InstallLocation")?.as_str()?;
    if loc.is_empty() {
        return None;
    }
    Some(EpicGame {
        name: v
            .get("DisplayName")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(loc)
            .to_string(),
        install_location: PathBuf::from(loc),
        size: v.get("InstallSize").and_then(serde_json::Value::as_u64),
    })
}

fn ini_sections(text: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{FEFF}');
        if line.starts_with(';') || line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push((name.to_string(), Vec::new()));
        } else if let Some((k, v)) = line.split_once('=')
            && let Some(last) = out.last_mut()
        {
            last.1.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    out
}

/// Firefox profile folders from `profiles.ini`. Relative paths resolve
/// against `base` (the folder holding `profiles.ini`).
///
/// # Example
///
/// ```
/// use std::path::Path;
/// use strata_classify::discover::firefox_profiles;
/// let ini = "[Profile0]\nIsRelative=1\nPath=Profiles/ab.default\n[Profile1]\nIsRelative=0\nPath=D:\\FF\n";
/// let p = firefox_profiles(ini, Path::new(r"C:\R\Mozilla\Firefox"));
/// assert_eq!(p.len(), 2);
/// assert!(p[0].ends_with("ab.default"));
/// ```
#[must_use]
pub fn firefox_profiles(ini: &str, base: &Path) -> Vec<PathBuf> {
    ini_sections(ini)
        .into_iter()
        .filter(|(name, _)| name.starts_with("Profile") || name.starts_with("Install"))
        .filter_map(|(name, kv)| {
            let get = |k: &str| kv.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
            let path = get(if name.starts_with("Install") {
                "Default"
            } else {
                "Path"
            })?;
            // [Install*] sections only carry relative Default= paths.
            let relative = get("IsRelative").is_none_or(|v| v == "1");
            let rel = path.replace('/', "\\");
            Some(if relative {
                base.join(rel)
            } else {
                PathBuf::from(rel)
            })
        })
        .fold(Vec::new(), |mut acc, p| {
            if !acc.contains(&p) {
                acc.push(p);
            }
            acc
        })
}

/// Recording output folders from an OBS profile's `basic.ini`.
#[must_use]
pub fn obs_recording_paths(ini: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for (_, kv) in ini_sections(ini) {
        for (k, v) in kv {
            if matches!(k.as_str(), "FilePath" | "RecFilePath" | "FFFilePath") && !v.is_empty() {
                // OBS escapes backslashes in its ini files.
                let p = PathBuf::from(v.replace(r"\\", r"\"));
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Blob file names (`sha256-<hex>`) referenced by an Ollama model manifest.
///
/// Manifests live at `models\manifests\<registry>\<ns>\<model>\<tag>`; blobs
/// at `models\blobs\sha256-<hex>`. Use this to attribute blobs to models.
#[must_use]
pub fn ollama_manifest_blobs(json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut push = |d: Option<&serde_json::Value>| {
        if let Some(d) = d.and_then(serde_json::Value::as_str) {
            out.push(d.replace(':', "-"));
        }
    };
    push(v.get("config").and_then(|c| c.get("digest")));
    if let Some(layers) = v.get("layers").and_then(serde_json::Value::as_array) {
        for l in layers {
            push(l.get("digest"));
        }
    }
    out
}

// -----------------------------------------------------------------------------
// Discovery
// -----------------------------------------------------------------------------

/// A game found through a launcher's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GameInstall {
    /// Launcher name.
    pub launcher: &'static str,
    /// Game name.
    pub name: String,
    /// Install folder.
    pub path: PathBuf,
    /// Launcher-reported size.
    pub size: Option<u64>,
}

/// A WSL distribution from the `Lxss` registry key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WslDistro {
    /// Distribution name.
    pub name: String,
    /// Folder holding its `ext4.vhdx`.
    pub base_path: PathBuf,
}

/// Everything [`discover`] found.
#[derive(Debug, Clone, Default)]
pub struct Discovered {
    /// Token roots for the classifier.
    pub roots: DynamicRoots,
    /// Games with launcher-provided names.
    pub games: Vec<GameInstall>,
    /// WSL distributions.
    pub wsl: Vec<WslDistro>,
    /// Ollama blob path → model name.
    pub ollama_blobs: Vec<(PathBuf, String)>,
    /// Files that existed but could not be read or parsed.
    pub warnings: Vec<String>,
}

fn read(path: &Path, warnings: &mut Vec<String>) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warnings.push(format!("{}: {e}", path.display()));
            None
        }
    }
}

fn strip_verbatim(p: &str) -> PathBuf {
    PathBuf::from(p.strip_prefix(r"\\?\").unwrap_or(p))
}

fn discover_steam(kf: &KnownFolders, d: &mut Discovered) {
    let mut libs: Vec<PathBuf> = Vec::new();
    for f in [KnownFolder::ProgramFilesX86, KnownFolder::ProgramFiles] {
        for (_, pf) in kf.resolve_all(f) {
            let steam = pf.join("Steam");
            if let Some(text) = read(
                &steam.join("steamapps").join("libraryfolders.vdf"),
                &mut d.warnings,
            ) {
                libs.extend(steam_libraries(&text));
                if !libs.contains(&steam) {
                    libs.push(steam);
                }
            }
        }
    }
    for lib in libs {
        d.roots.insert("STEAM_LIBRARY", &lib);
        let apps = lib.join("steamapps");
        let Ok(rd) = std::fs::read_dir(&apps) else {
            continue;
        };
        for e in rd.filter_map(Result::ok) {
            let name = e.file_name().to_string_lossy().into_owned();
            if !(name.starts_with("appmanifest_") && name.ends_with(".acf")) {
                continue;
            }
            if let Some(app) = read(&e.path(), &mut d.warnings).and_then(|t| steam_app_manifest(&t))
            {
                d.games.push(GameInstall {
                    launcher: "Steam",
                    name: app.name,
                    path: apps.join("common").join(&app.installdir),
                    size: app.size_on_disk,
                });
            }
        }
    }
}

fn discover_epic(kf: &KnownFolders, d: &mut Discovered) {
    for (_, pd) in kf.resolve_all(KnownFolder::ProgramData) {
        let dir = pd.join(r"Epic\EpicGamesLauncher\Data\Manifests");
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.filter_map(Result::ok) {
            if e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("item"))
                && let Some(g) = read(&e.path(), &mut d.warnings).and_then(|t| epic_manifest(&t))
            {
                d.roots.insert("EPIC_GAME", &g.install_location);
                d.games.push(GameInstall {
                    launcher: "Epic Games",
                    name: g.name,
                    path: g.install_location,
                    size: g.size,
                });
            }
        }
    }
}

fn discover_user_configs(kf: &KnownFolders, d: &mut Discovered) {
    for (_, appdata) in kf.resolve_all(KnownFolder::AppData) {
        let ff = appdata.join(r"Mozilla\Firefox");
        if let Some(ini) = read(&ff.join("profiles.ini"), &mut d.warnings) {
            for p in firefox_profiles(&ini, &ff) {
                d.roots.insert("FIREFOX_PROFILE", p);
            }
        }
        let obs = appdata.join(r"obs-studio\basic\profiles");
        if let Ok(rd) = std::fs::read_dir(&obs) {
            for e in rd.filter_map(Result::ok) {
                if let Some(ini) = read(&e.path().join("basic.ini"), &mut d.warnings) {
                    for p in obs_recording_paths(&ini) {
                        d.roots.insert("OBS_RECORDINGS", p);
                    }
                }
            }
        }
    }
}

fn discover_env(kf: &KnownFolders, d: &mut Discovered) {
    for var in [
        "HF_HOME",
        "OLLAMA_MODELS",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "GOMODCACHE",
    ] {
        if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            d.roots.insert(var, PathBuf::from(v));
        }
    }
    let models: Vec<PathBuf> = d
        .roots
        .get("OLLAMA_MODELS")
        .iter()
        .cloned()
        .chain(
            kf.resolve_all(KnownFolder::UserProfile)
                .map(|(_, p)| p.join(r".ollama\models")),
        )
        .collect();
    for m in models {
        let manifests = m.join("manifests");
        let mut stack = vec![(manifests.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.filter_map(Result::ok) {
                let p = e.path();
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    if depth < 6 {
                        stack.push((p, depth + 1));
                    }
                } else if let Some(text) = read(&p, &mut d.warnings) {
                    let rel = p.strip_prefix(&manifests).unwrap_or(&p);
                    let parts: Vec<String> = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect();
                    // registry/namespace/model/tag → "namespace/model:tag"
                    let model = match parts.as_slice() {
                        [.., ns, model, tag] if ns == "library" => format!("{model}:{tag}"),
                        [.., ns, model, tag] => format!("{ns}/{model}:{tag}"),
                        _ => rel.display().to_string(),
                    };
                    for blob in ollama_manifest_blobs(&text) {
                        d.ollama_blobs
                            .push((m.join("blobs").join(blob), model.clone()));
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn discover_wsl(d: &mut Discovered) {
    let Ok(lxss) =
        windows_registry::CURRENT_USER.open(r"Software\Microsoft\Windows\CurrentVersion\Lxss")
    else {
        return;
    };
    let Ok(keys) = lxss.keys() else { return };
    for k in keys {
        let Ok(dk) = lxss.open(&k) else { continue };
        let (Ok(name), Ok(base)) = (dk.get_string("DistributionName"), dk.get_string("BasePath"))
        else {
            continue;
        };
        let base = strip_verbatim(&base);
        d.roots.insert("WSL_DISTRO", &base);
        d.wsl.push(WslDistro {
            name,
            base_path: base,
        });
    }
}

#[cfg(not(windows))]
fn discover_wsl(_: &mut Discovered) {}

/// Reads launcher and app configuration for every resolved profile and
/// returns the token roots and metadata found. Read-only; missing files are
/// skipped silently, unreadable ones are reported in `warnings`.
#[must_use]
pub fn discover(kf: &KnownFolders) -> Discovered {
    let mut d = Discovered::default();
    discover_steam(kf, &mut d);
    discover_epic(kf, &mut d);
    discover_user_configs(kf, &mut d);
    discover_env(kf, &mut d);
    discover_wsl(&mut d);
    let _ = strip_verbatim;
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIBRARY: &str = r#""libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"apps" { "228980" "0" }
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
	}
}"#;

    #[test]
    fn steam_parsing() {
        let libs = steam_libraries(LIBRARY);
        assert_eq!(
            libs,
            [
                PathBuf::from(r"C:\Program Files (x86)\Steam"),
                PathBuf::from(r"D:\SteamLibrary")
            ]
        );
        let old = "\"LibraryFolders\" { \"TimeNextStatsReport\" \"1\" \"1\" \"E:\\\\Games\" }";
        assert_eq!(steam_libraries(old), [PathBuf::from(r"E:\Games")]);
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"252490\"\n\t\"name\"\t\t\"Rust\"\n\t\"installdir\"\t\t\"Rust\"\n\t\"SizeOnDisk\"\t\t\"30000\"\n}";
        let app = steam_app_manifest(acf).unwrap();
        assert_eq!((app.name.as_str(), app.size_on_disk), ("Rust", Some(30000)));
        assert!(steam_app_manifest("garbage { { }").is_none());
        // Deep nesting and unterminated input must not panic.
        let deep = "\"a\" {".repeat(10_000);
        let _ = parse_kv(&deep);
        let _ = parse_kv("\"unterminated");
    }

    #[test]
    fn epic_parsing() {
        let g = epic_manifest(r#"{"DisplayName":"Fortnite","InstallLocation":"C:\\Program Files\\Epic Games\\Fortnite","InstallSize":1234}"#).unwrap();
        assert_eq!(g.name, "Fortnite");
        assert_eq!(g.size, Some(1234));
        assert!(epic_manifest("{}").is_none());
        assert!(epic_manifest("nope").is_none());
    }

    #[test]
    fn obs_and_firefox() {
        let ini = "[SimpleOutput]\nFilePath=C:\\\\Users\\\\me\\\\Videos\n[AdvOut]\nRecFilePath=D:\\\\Rec\nFFFilePath=C:\\\\Users\\\\me\\\\Videos\n";
        assert_eq!(
            obs_recording_paths(ini),
            [
                PathBuf::from(r"C:\Users\me\Videos"),
                PathBuf::from(r"D:\Rec")
            ]
        );
        let ff = "[Install308046B0AF4A39CB]\nDefault=Profiles/x.default-release\n[Profile0]\nPath=Profiles/x.default-release\nIsRelative=1\n";
        assert_eq!(firefox_profiles(ff, Path::new(r"C:\ff")).len(), 1);
    }

    #[test]
    fn ollama_blobs() {
        let m = r#"{"config":{"digest":"sha256:aa"},"layers":[{"digest":"sha256:bb"},{"digest":"sha256:cc"}]}"#;
        assert_eq!(
            ollama_manifest_blobs(m),
            ["sha256-aa", "sha256-bb", "sha256-cc"]
        );
        assert!(ollama_manifest_blobs("{").is_empty());
    }

    #[test]
    fn verbatim_paths() {
        assert_eq!(strip_verbatim(r"\\?\C:\x"), PathBuf::from(r"C:\x"));
    }
}
