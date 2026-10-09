//! Build script: Tauri's codegen, plus the third-party license list the
//! About screen shows (`about_licenses`).
//!
//! The list is built from what is on disk at build time: `Cargo.lock`
//! (every crate reachable from the app and the helper, with the license and
//! repository from each crate's manifest in the Cargo registry) and the UI's
//! production npm dependencies (from `ui/node_modules`). A source that cannot
//! be read contributes nothing; the build never fails because of it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=../Cargo.lock");
    println!("cargo:rerun-if-changed=../ui/package.json");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let mut list = cargo_licenses(Path::new("../Cargo.lock"));
    list.extend(npm_licenses(Path::new("../ui")));
    let json = serde_json::to_string(&list).unwrap_or_else(|_| "[]".into());
    std::fs::write(out.join("licenses.json"), json).expect("write licenses.json");
    tauri_build::build();
}

fn entry(
    name: &str,
    version: &str,
    license: Option<String>,
    eco: &str,
    repo: Option<String>,
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "version": version,
        "license": license.unwrap_or_else(|| "see the package".into()),
        "ecosystem": eco,
        "repository": repo,
    })
}

struct LockPackage {
    version: String,
    registry: bool,
    deps: Vec<String>,
}

fn parse_lock(text: &str) -> BTreeMap<(String, String), LockPackage> {
    let mut out = BTreeMap::new();
    for block in text.split("[[package]]").skip(1) {
        let mut name = String::new();
        let mut pkg = LockPackage {
            version: String::new(),
            registry: false,
            deps: Vec::new(),
        };
        let mut in_deps = false;
        for line in block.lines() {
            let line = line.trim();
            if in_deps {
                if line.starts_with(']') {
                    in_deps = false;
                } else if let Some(d) = line.trim_end_matches(',').strip_prefix('"') {
                    pkg.deps.push(d.trim_end_matches('"').to_owned());
                }
                continue;
            }
            if let Some(v) = value(line, "name") {
                name = v;
            } else if let Some(v) = value(line, "version") {
                pkg.version = v;
            } else if let Some(v) = value(line, "source") {
                pkg.registry = v.starts_with("registry+");
            } else if line.starts_with("dependencies = [") {
                in_deps = !line.ends_with(']');
            }
        }
        if !name.is_empty() {
            out.insert((name, pkg.version.clone()), pkg);
        }
    }
    out
}

fn value(line: &str, key: &str) -> Option<String> {
    let rest = line
        .strip_prefix(key)?
        .trim_start()
        .strip_prefix('=')?
        .trim();
    Some(rest.trim_matches('"').to_owned())
}

fn registry_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".cargo")));
    let Some(src) = home.map(|h| h.join("registry").join("src")) else {
        return Vec::new();
    };
    std::fs::read_dir(src)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// `license` and `repository` from a crate manifest's `[package]` table.
fn manifest_facts(dirs: &[PathBuf], name: &str, version: &str) -> (Option<String>, Option<String>) {
    for d in dirs {
        let Ok(text) =
            std::fs::read_to_string(d.join(format!("{name}-{version}")).join("Cargo.toml"))
        else {
            continue;
        };
        let mut in_package = false;
        let (mut license, mut repo) = (None, None);
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_package = line == "[package]";
                continue;
            }
            if in_package {
                if let Some(v) = value(line, "license") {
                    license = Some(v);
                } else if let Some(v) = value(line, "repository") {
                    repo = Some(v);
                }
            }
        }
        return (license, repo);
    }
    (None, None)
}

fn cargo_licenses(lock: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(lock) else {
        return Vec::new();
    };
    let packages = parse_lock(&text);
    let by_name: BTreeMap<&str, Vec<&(String, String)>> =
        packages.keys().fold(BTreeMap::new(), |mut m, k| {
            m.entry(k.0.as_str()).or_insert_with(Vec::new).push(k);
            m
        });
    let resolve = |dep: &str| -> Option<(String, String)> {
        let mut parts = dep.split(' ');
        let name = parts.next()?;
        let candidates = by_name.get(name)?;
        match parts.next() {
            Some(v) => candidates.iter().find(|k| k.1 == v).map(|k| (*k).clone()),
            None => candidates.first().map(|k| (*k).clone()),
        }
    };
    let mut seen = BTreeSet::new();
    let mut queue: VecDeque<(String, String)> = ["strata-app", "strata-helper"]
        .iter()
        .filter_map(|n| resolve(n))
        .collect();
    while let Some(k) = queue.pop_front() {
        if !seen.insert(k.clone()) {
            continue;
        }
        if let Some(p) = packages.get(&k) {
            queue.extend(p.deps.iter().filter_map(|d| resolve(d)));
        }
    }
    let dirs = registry_dirs();
    seen.iter()
        .filter_map(|k| {
            let p = packages.get(k)?;
            if !p.registry {
                return None;
            }
            let (license, repo) = manifest_facts(&dirs, &k.0, &k.1);
            Some(entry(&k.0, &k.1, license, "cargo", repo))
        })
        .collect()
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn npm_licenses(ui: &Path) -> Vec<serde_json::Value> {
    let Some(root) = read_json(&ui.join("package.json")) else {
        return Vec::new();
    };
    let mut queue: VecDeque<PathBuf> = root["dependencies"]
        .as_object()
        .map(|o| o.keys().map(|k| ui.join("node_modules").join(k)).collect())
        .unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    while let Some(dir) = queue.pop_front() {
        // NOTE: pnpm links each package into `.pnpm/<id>/node_modules/<name>`
        // with its own dependencies as siblings, so the real path tells where
        // to look for them.
        let Ok(real) = std::fs::canonicalize(&dir) else {
            continue;
        };
        let Some(pkg) = read_json(&real.join("package.json")) else {
            continue;
        };
        let name = pkg["name"].as_str().unwrap_or_default().to_owned();
        let version = pkg["version"].as_str().unwrap_or_default().to_owned();
        if name.is_empty() || !seen.insert((name.clone(), version.clone())) {
            continue;
        }
        let license = pkg["license"].as_str().map(str::to_owned);
        let repo = match &pkg["repository"] {
            serde_json::Value::String(s) => Some(s.clone()),
            v => v["url"].as_str().map(str::to_owned),
        };
        out.push(entry(&name, &version, license, "npm", repo));
        let base = if name.contains('/') {
            real.parent().and_then(Path::parent)
        } else {
            real.parent()
        };
        if let (Some(deps), Some(base)) = (pkg["dependencies"].as_object(), base) {
            queue.extend(deps.keys().map(|k| base.join(k)));
        }
    }
    out
}
