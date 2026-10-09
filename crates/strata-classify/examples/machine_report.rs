//! Read-only report on the current machine: installed-apps catalog,
//! discovered rule roots, orphan candidates, and how real paths classify.
//!
//! Run with `cargo run -p strata-classify --example machine_report`.
//! Known folders come from environment variables here only because this is a
//! developer tool; the app resolves them with `SHGetKnownFolderPath`.

use std::path::{Path, PathBuf};

use strata_classify::catalog::{AppCatalog, InstalledApp};
use strata_classify::{Classifier, RuleSet, StdFsProbe, discover};
use strata_core::known::{KnownFolder, KnownFolders, UserFolders};

fn env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn known_folders() -> KnownFolders {
    let mut kf = KnownFolders::default();
    let windir = env("WINDIR").unwrap_or_else(|| r"C:\Windows".into());
    kf.machine.insert(KnownFolder::Windir, windir);
    for (k, v) in [
        (KnownFolder::ProgramFiles, "ProgramFiles"),
        (KnownFolder::ProgramFilesX86, "ProgramFiles(x86)"),
        (KnownFolder::ProgramData, "ProgramData"),
        (KnownFolder::Public, "PUBLIC"),
    ] {
        if let Some(p) = env(v) {
            kf.machine.insert(k, p);
        }
    }
    let home = env("USERPROFILE").unwrap_or_default();
    kf.machine.insert(
        KnownFolder::UserProfiles,
        home.parent().map(Path::to_path_buf).unwrap_or_default(),
    );
    let mut u = UserFolders {
        is_current: true,
        ..Default::default()
    };
    u.folders.insert(KnownFolder::UserProfile, home.clone());
    for (k, v) in [
        (KnownFolder::LocalAppData, "LOCALAPPDATA"),
        (KnownFolder::AppData, "APPDATA"),
        (KnownFolder::Temp, "TEMP"),
    ] {
        if let Some(p) = env(v) {
            u.folders.insert(k, p);
        }
    }
    // OneDrive folder backup redirects Documents on this machine; the app
    // gets the real location from the known-folder API.
    let docs = home.join(r"OneDrive\Documents");
    u.folders.insert(
        KnownFolder::Documents,
        if docs.exists() {
            docs
        } else {
            home.join("Documents")
        },
    );
    for (k, n) in [
        (KnownFolder::Downloads, "Downloads"),
        (KnownFolder::Desktop, "Desktop"),
        (KnownFolder::Pictures, "Pictures"),
        (KnownFolder::Music, "Music"),
        (KnownFolder::Videos, "Videos"),
    ] {
        u.folders.insert(k, home.join(n));
    }
    kf.users.push(u);
    kf
}

/// Days since the newest modification in `dir`'s subtree (bounded walk; the
/// app gets this from its index). Unknown (too large to walk) counts as active.
fn idle_days(dir: &Path) -> u32 {
    let mut newest = std::time::UNIX_EPOCH;
    let mut stack = vec![(dir.to_path_buf(), 0)];
    let mut seen = 0;
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.filter_map(Result::ok) {
            seen += 1;
            if seen > 20_000 {
                return 0;
            }
            if let Ok(md) = e.metadata() {
                if let Ok(m) = md.modified() {
                    newest = newest.max(m);
                }
                if md.is_dir() && depth < 8 {
                    stack.push((e.path(), depth + 1));
                }
            }
        }
    }
    let age = std::time::SystemTime::now()
        .duration_since(newest)
        .unwrap_or_default();
    u32::try_from(age.as_secs() / 86_400).unwrap_or(u32::MAX)
}

fn main() {
    let kf = known_folders();
    let local = env("LOCALAPPDATA").unwrap_or_default();
    let roaming = env("APPDATA").unwrap_or_default();
    let home = env("USERPROFILE").unwrap_or_default();

    // --- Catalog -------------------------------------------------------------
    let t0 = std::time::Instant::now();
    let discovered = discover::discover(&kf);
    let (mut apps, warnings) = AppCatalog::read_system(&kf);
    apps.extend(discovered.games.iter().map(InstalledApp::from_game));
    let catalog = AppCatalog::new(apps, &kf);
    println!(
        "== Installed-apps catalog: {} records in {:?}",
        catalog.apps().len(),
        t0.elapsed()
    );
    let mut by_source = std::collections::BTreeMap::new();
    for a in catalog.apps() {
        *by_source
            .entry(
                format!("{:?}", a.source)
                    .split(' ')
                    .next()
                    .unwrap_or("")
                    .to_string(),
            )
            .or_insert(0) += 1;
    }
    println!("   by source: {by_source:?}");
    for w in warnings.iter().take(5) {
        println!("   warning: {w}");
    }
    let mut visible: Vec<_> = catalog
        .apps()
        .iter()
        .filter(|a| !a.system_component)
        .collect();
    visible.sort_by_key(|a| std::cmp::Reverse(a.estimated_size));
    println!("   largest by registry estimate:");
    for a in visible.iter().take(12) {
        println!(
            "   - {:<40} {:<28} {:>8} MB  {}",
            a.name.chars().take(40).collect::<String>(),
            a.publisher
                .as_deref()
                .unwrap_or("-")
                .chars()
                .take(28)
                .collect::<String>(),
            a.estimated_size.unwrap_or(0) / 1_000_000,
            a.install_location
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );
    }

    // --- Attribution ---------------------------------------------------------
    println!("\n== Attribution samples");
    let samples = [
        local.join(r"Discord\app-1.0.9261"),
        roaming.join(r"obs-studio\basic"),
        roaming.join(r"Telegram Desktop\tdata"),
        local.join(r"Google\Chrome\User Data\Default"),
        local.join(r"Programs\Microsoft VS Code\Code.exe"),
        local.join(r"Packages\MSTeams_8wekyb3d8bbwe\LocalCache"),
        roaming.join(r"Ledger Live"),
        local.join(r"Docker\wsl"),
        local.join(r"NVIDIA\DXCache"),
        home.join(r".claude\projects"),
        PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\common\Rust"),
    ];
    let c = Classifier::new(&RuleSet::builtin().unwrap(), &kf, &discovered.roots).unwrap();
    for p in &samples {
        let ps = p.to_string_lossy();
        let cls = c.classify_path(&ps, &strata_classify::Entry::dir("x"));
        let label = cls.rule.and_then(|r| {
            let rule = c.rule(r);
            rule.app.as_deref().map(|a| (rule.id.as_str(), a))
        });
        match catalog.attribute(&ps, label) {
            Some(a) => println!(
                "   {ps}\n      -> {} [{:?}] {:?}",
                a.label,
                a.confidence,
                a.evidence.first()
            ),
            None => println!("   {ps}\n      -> (unattributed)"),
        }
    }

    // --- Orphans -------------------------------------------------------------
    println!(
        "\n== Orphan candidates (first-level folders in LocalAppData / Roaming / ProgramData)"
    );
    let mut orphans = Vec::new();
    for root in [
        local.clone(),
        roaming.clone(),
        env("ProgramData").unwrap_or_default(),
    ] {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in rd.filter_map(Result::ok) {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let p = e.path();
            let ps = p.to_string_lossy();
            let cls = c.classify_path(&ps, &strata_classify::Entry::dir("x"));
            let app = cls.rule.and_then(|r| c.rule(r).app.clone());
            if let Some(o) = catalog.orphan_check(&ps, app.as_deref(), idle_days(&p)) {
                orphans.push(o.folder);
            }
        }
    }
    println!("   {} candidates; first 25:", orphans.len());
    for o in orphans.iter().take(25) {
        println!("   - {o}");
    }

    // --- Discovery -----------------------------------------------------------
    let d = &discovered;
    println!("\n== Discovered roots");
    for (t, roots) in d.roots.iter() {
        println!("   {{{t}}} = {roots:?}");
    }
    for g in &d.games {
        println!(
            "   game: {} ({}) at {}",
            g.name,
            g.launcher,
            g.path.display()
        );
    }
    for w in &d.wsl {
        println!("   wsl: {} at {}", w.name, w.base_path.display());
    }
    println!("   ollama blobs attributed: {}", d.ollama_blobs.len());

    // --- Real-path classification ---------------------------------------------
    println!("\n== Real paths (exists | rule | safety)");
    let real = [
        local.join("Temp"),
        local.join("npm-cache"),
        roaming.join("npm-cache"),
        local.join(r"pnpm\store"),
        local.join("pnpm-cache"),
        local.join(r"pip\cache"),
        home.join(r".cargo\registry"),
        home.join(r".cargo\git"),
        home.join(r".rustup\toolchains"),
        home.join(r"go\pkg\mod"),
        local.join("go-build"),
        home.join(r".gradle\caches"),
        home.join(r".nuget\packages"),
        local.join(r"NuGet\v3-cache"),
        local.join(r"Android\Sdk\system-images"),
        PathBuf::from(r"C:\ProgramData\Microsoft\VisualStudio\Packages"),
        local.join("ms-playwright"),
        local.join("nvm"),
        local.join(r"Docker\wsl\disk\docker_data.vhdx"),
        local.join(r"Docker\wsl\main\ext4.vhdx"),
        local.join(r"Google\Chrome\User Data"),
        local.join(r"Google\Chrome\User Data\Default\Cache"),
        local.join(r"Google\Chrome\User Data\Default\Code Cache"),
        local.join(r"Google\Chrome\User Data\Default\Service Worker\CacheStorage"),
        local.join(r"Google\Chrome\User Data\ShaderCache"),
        local.join(r"Microsoft\Edge\User Data\Default\Cache"),
        roaming.join(r"Mozilla\Firefox\Profiles"),
        local.join(r"Mozilla\Firefox\Profiles"),
        roaming.join(r"discord\Cache"),
        roaming.join(r"discord\Code Cache"),
        roaming.join(r"discord\Local Storage"),
        local.join("Discord"),
        roaming.join(r"Code\CachedData"),
        roaming.join(r"Code\User"),
        home.join(r".vscode\extensions"),
        roaming.join(r"Telegram Desktop\tdata"),
        roaming.join("Zoom"),
        local.join(r"Packages\MSTeams_8wekyb3d8bbwe"),
        home.join(".claude"),
        home.join(r".claude\projects"),
        home.join(r".claude\file-history"),
        home.join(r".claude\cache"),
        home.join(r".claude\plugins"),
        home.join(r".claude\settings.json"),
        home.join(".claude.json"),
        home.join(r".local\bin\claude.exe"),
        home.join(r".local\share\claude\versions"),
        home.join(r".local\state\claude"),
        local.join("claude-cli-nodejs"),
        home.join(".copilot"),
        roaming.join("TabNine"),
        local.join("D3DSCache"),
        local.join(r"NVIDIA\DXCache"),
        local.join(r"NVIDIA\GLCache"),
        local.join("CrashDumps"),
        local.join(r"Microsoft\Windows\Explorer"),
        PathBuf::from(r"C:\ProgramData\Microsoft\Windows\WER"),
        PathBuf::from(r"C:\Windows\SoftwareDistribution\Download"),
        PathBuf::from(r"C:\Windows\Temp"),
        PathBuf::from(r"C:\Windows\Prefetch"),
        PathBuf::from(r"C:\Windows\Logs\CBS"),
        PathBuf::from(r"C:\Windows\WinSxS"),
        PathBuf::from(r"C:\Windows\Installer"),
        PathBuf::from(r"C:\Windows\Minidump"),
        PathBuf::from(r"C:\$Recycle.Bin"),
        PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\common"),
        PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\shadercache"),
        PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\downloading"),
        local.join(r"Steam\htmlcache"),
        PathBuf::from(r"C:\XboxGames"),
        local.join(r"Roblox\Versions"),
        roaming.join(r"obs-studio\logs"),
        home.join("Downloads"),
    ];
    for p in &real {
        let ps = p.to_string_lossy();
        let exists = p.exists();
        let e = c.explain(&ps, &StdFsProbe);
        let id = e.rule.as_ref().map_or("-".to_string(), |r| r.id.clone());
        println!(
            "   {} | {:<38} | {:<8} | {}",
            if exists { "yes" } else { "NO " },
            id,
            format!("{:?}", e.result.safety),
            ps
        );
    }
    if !c.unresolved_tokens().is_empty() {
        println!(
            "\n   unresolved tokens: {}",
            c.unresolved_tokens().join(", ")
        );
    }
}
