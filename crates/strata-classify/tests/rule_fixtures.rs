//! Fixture tests for every built-in rule (SPEC §23 M6: "every built-in rule
//! has a fixture test").
//!
//! Each case in `tests/fixtures/rules.toml` builds a small synthetic tree,
//! walks it top-down through the real classifier API (`root` →
//! `enter_dir` / `classify_file`) and asserts that the rule matches its
//! intended paths and does not match near-miss paths.
//!
//! The machine layout has two profiles: `alice` (standard English layout)
//! and `bob` (localized and redirected folders: Documents on `D:\Docs`,
//! Downloads on `D:\Téléchargements`, Temp on `D:\Temp\bob`, Desktop and
//! Pictures inside OneDrive). Paths written with a per-user token are
//! checked for both profiles, including that the result is attributed to the
//! right profile.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Deserialize;
use strata_classify::fold::{normalize_path, path_components_raw};
use strata_classify::schema::parse_size;
use strata_classify::{
    ChildRef, Classification, Classifier, DetectedType, DirScope, DynamicRoots, Entry, FsProbe,
    Name, ProbeEntry, RuleSet,
};
use strata_core::known::{KnownFolder, KnownFolders, UserFolders};
use strata_core::{CloudState, EntryFlags, FileTime};

const NOW: i64 = 1_790_000_000;
const DEFAULT_FILE_SIZE: u64 = 1000;
const DEFAULT_AGE_DAYS: u32 = 1;

// -----------------------------------------------------------------------------
// Machine layout
// -----------------------------------------------------------------------------

fn user(name: &str, sid: &str, current: bool, overrides: &[(KnownFolder, &str)]) -> UserFolders {
    let home = format!(r"C:\Users\{name}");
    let mut u = UserFolders {
        sid: Some(sid.to_string()),
        account: Some(name.to_string()),
        is_current: current,
        folders: Default::default(),
    };
    let defaults = [
        (KnownFolder::UserProfile, home.clone()),
        (KnownFolder::LocalAppData, format!(r"{home}\AppData\Local")),
        (KnownFolder::AppData, format!(r"{home}\AppData\Roaming")),
        (KnownFolder::Temp, format!(r"{home}\AppData\Local\Temp")),
        (KnownFolder::Downloads, format!(r"{home}\Downloads")),
        (KnownFolder::Documents, format!(r"{home}\Documents")),
        (KnownFolder::Desktop, format!(r"{home}\Desktop")),
        (KnownFolder::Pictures, format!(r"{home}\Pictures")),
        (KnownFolder::Music, format!(r"{home}\Music")),
        (KnownFolder::Videos, format!(r"{home}\Videos")),
    ];
    for (k, v) in defaults {
        u.folders.insert(k, PathBuf::from(v));
    }
    for (k, v) in overrides {
        u.folders.insert(*k, PathBuf::from(*v));
    }
    u
}

fn known_folders() -> KnownFolders {
    let mut kf = KnownFolders::default();
    for (k, v) in [
        (KnownFolder::Windir, r"C:\Windows"),
        (KnownFolder::ProgramFiles, r"C:\Program Files"),
        (KnownFolder::ProgramFilesX86, r"C:\Program Files (x86)"),
        (KnownFolder::ProgramData, r"C:\ProgramData"),
        (KnownFolder::UserProfiles, r"C:\Users"),
        (KnownFolder::Public, r"C:\Users\Public"),
    ] {
        kf.machine.insert(k, PathBuf::from(v));
    }
    kf.users.push(user("alice", "S-1-5-21-1-1001", true, &[]));
    kf.users.push(user(
        "bob",
        "S-1-5-21-1-1002",
        false,
        &[
            (KnownFolder::Documents, r"D:\Docs"),
            (KnownFolder::Downloads, r"D:\Téléchargements"),
            (KnownFolder::Temp, r"D:\Temp\bob"),
            (KnownFolder::Desktop, r"C:\Users\bob\OneDrive\Bureau"),
            (KnownFolder::Pictures, r"C:\Users\bob\OneDrive\Images"),
            (KnownFolder::Music, r"C:\Users\bob\Musique"),
            (KnownFolder::Videos, r"C:\Users\bob\Vidéos"),
        ],
    ));
    kf
}

fn dynamic_roots() -> DynamicRoots {
    let mut d = DynamicRoots::default();
    for (t, p) in [
        ("STEAM_LIBRARY", r"D:\SteamLibrary"),
        ("EPIC_GAME", r"E:\Epic\Fortnite"),
        ("FIREFOX_PROFILE", r"E:\FirefoxProfiles\custom"),
        ("OBS_RECORDINGS", r"E:\Rec"),
        ("WSL_DISTRO", r"E:\WSL\Ubuntu"),
        ("HF_HOME", r"E:\hf"),
        ("OLLAMA_MODELS", r"E:\ollama"),
        ("CARGO_HOME", r"E:\cargo"),
        ("RUSTUP_HOME", r"E:\rustup"),
        ("GOMODCACHE", r"E:\gomod"),
    ] {
        d.insert(t, p);
    }
    d
}

fn classifier() -> Classifier {
    let rules = RuleSet::builtin().expect("built-in packs are valid");
    let mut c = Classifier::new(&rules, &known_folders(), &dynamic_roots()).expect("compiles");
    c.set_now(NOW);
    c
}

/// Expands a leading token into one path per resolution, paired with the
/// owning profile index for per-user tokens.
fn expand(path: &str, kf: &KnownFolders, dynamic: &DynamicRoots) -> Vec<(String, Option<u16>)> {
    let Some(rest) = path.strip_prefix('{') else {
        return vec![(path.to_string(), None)];
    };
    let close = rest.find('}').expect("closed token");
    let token = &rest[..close];
    let tail = &rest[close + 1..];
    let roots: Vec<(String, Option<u16>)> = if token == "SYSTEMDRIVE" {
        vec![("C:".to_string(), None)]
    } else if let Some(f) = KnownFolder::from_token(&format!("{{{token}}}")) {
        kf.resolve_all(f)
            .map(|(u, p)| {
                let idx = u.and_then(|u| kf.users.iter().position(|x| std::ptr::eq(x, u)));
                (
                    p.to_string_lossy().into_owned(),
                    idx.map(|i| u16::try_from(i).unwrap()),
                )
            })
            .collect()
    } else {
        dynamic
            .get(token)
            .iter()
            .map(|p| (p.to_string_lossy().into_owned(), None))
            .collect()
    };
    assert!(!roots.is_empty(), "token {{{token}}} has no fixture root");
    roots
        .into_iter()
        .map(|(r, u)| (format!("{r}{tail}"), u))
        .collect()
}

// -----------------------------------------------------------------------------
// Fixture format
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Fixtures {
    #[serde(rename = "case")]
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    rule: String,
    #[serde(default)]
    tree: Vec<TreeItem>,
    #[serde(rename = "match")]
    matches: Vec<String>,
    #[serde(default)]
    no_match: Vec<String>,
    /// Also assert that `<path>_nearmiss` (same kind and size) does not
    /// match. Disabled for wildcard rules where the near miss legitimately
    /// matches; those cases list explicit `no_match` paths instead.
    #[serde(default = "yes")]
    auto_near_miss: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TreeItem {
    Path(String),
    Spec(Spec),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    path: String,
    #[serde(default)]
    size: Option<toml::Value>,
    #[serde(default)]
    age_days: Option<u32>,
    #[serde(default)]
    magic: Option<String>,
    #[serde(default)]
    flags: Vec<String>,
}

impl TreeItem {
    fn spec(&self) -> Spec {
        match self {
            Self::Path(p) => Spec {
                path: p.clone(),
                size: None,
                age_days: None,
                magic: None,
                flags: vec![],
            },
            Self::Spec(s) => s.clone(),
        }
    }
}

// -----------------------------------------------------------------------------
// Synthetic filesystem
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Attrs {
    size: Option<u64>,
    age_days: Option<u32>,
    magic: Option<DetectedType>,
    flags: EntryFlags,
}

#[derive(Debug, Default)]
struct Node {
    name: String,
    is_dir: bool,
    attrs: Attrs,
    children: BTreeMap<String, Node>,
    // Aggregates filled by `finish`.
    total: u64,
    newest_age: u32,
}

impl Node {
    fn insert(&mut self, comps: &[String], is_dir: bool, attrs: Attrs) {
        let Some((first, rest)) = comps.split_first() else {
            return;
        };
        let key = normalize_path(first);
        let child = self.children.entry(key).or_insert_with(|| Node {
            name: first.clone(),
            is_dir: true,
            ..Default::default()
        });
        if rest.is_empty() {
            child.is_dir = is_dir || !child.children.is_empty();
            child.attrs = attrs;
        } else {
            child.is_dir = true;
            child.insert(rest, is_dir, attrs);
        }
    }

    fn finish(&mut self, inherited_age: u32) {
        let age = self.attrs.age_days.unwrap_or(inherited_age);
        if self.is_dir {
            let mut total = 0u64;
            let mut newest = age;
            for c in self.children.values_mut() {
                c.finish(age);
                total += c.total;
                newest = newest.min(c.newest_age);
            }
            self.total = self.attrs.size.unwrap_or(total);
            self.newest_age = if self.children.is_empty() {
                age
            } else {
                newest
            };
        } else {
            self.total = self.attrs.size.unwrap_or(DEFAULT_FILE_SIZE);
            self.newest_age = age;
        }
    }

    fn entry(&self) -> Entry<'_> {
        let mut flags = self.attrs.flags;
        flags.set(EntryFlags::DIR, self.is_dir);
        Entry {
            name: Name::Str(&self.name),
            flags,
            size: self.total,
            newest_mtime: Some(FileTime::from_unix_secs(
                NOW - i64::from(self.newest_age) * 86_400,
            )),
            magic: self.attrs.magic,
        }
    }

    fn child_refs(&self) -> impl Iterator<Item = ChildRef<'_>> {
        self.children.values().map(|c| ChildRef {
            name: Name::Str(&c.name),
            is_dir: c.is_dir,
        })
    }

    fn find(&self, comps: &[String]) -> Option<&Node> {
        match comps.split_first() {
            None => Some(self),
            Some((first, rest)) => self.children.get(&normalize_path(first))?.find(rest),
        }
    }
}

fn parse_flags(flags: &[String]) -> EntryFlags {
    let mut f = EntryFlags::EMPTY;
    for s in flags {
        f = match s.as_str() {
            "ntfs_metadata" => f.union(EntryFlags::NTFS_METADATA),
            "access_denied" => f.union(EntryFlags::ACCESS_DENIED),
            "cloud" => f.with_cloud(CloudState::LocallyAvailable),
            "cloud_online_only" => f.with_cloud(CloudState::OnlineOnly),
            other => panic!("unknown fixture flag {other}"),
        };
    }
    f
}

fn attrs_of(spec: &Spec) -> Attrs {
    Attrs {
        size: spec.size.as_ref().map(|v| match v {
            toml::Value::Integer(i) => u64::try_from(*i).unwrap(),
            toml::Value::String(s) => parse_size(s).unwrap(),
            other => panic!("bad size {other:?}"),
        }),
        age_days: spec.age_days,
        magic: spec
            .magic
            .as_deref()
            .map(|m| DetectedType::from_name(m).expect("magic name")),
        flags: parse_flags(&spec.flags),
    }
}

fn walk(
    c: &Classifier,
    scope: &DirScope,
    node: &Node,
    path: &str,
    out: &mut BTreeMap<String, Classification>,
) {
    for child in node.children.values() {
        let p = format!(r"{path}\{}", child.name);
        let entry = child.entry();
        if child.is_dir {
            let s = c.enter_dir(scope, &entry, child.child_refs());
            out.insert(normalize_path(&p), s.classification());
            walk(c, &s, child, &p, out);
        } else {
            out.insert(normalize_path(&p), c.classify_file(scope, &entry));
        }
    }
}

struct Fs(Node);

impl Fs {
    fn classify_all(&self, c: &Classifier) -> BTreeMap<String, Classification> {
        let mut out = BTreeMap::new();
        for vol in self.0.children.values() {
            let root = c.root(&vol.name, vol.child_refs());
            walk(c, &root, vol, &vol.name, &mut out);
        }
        out
    }
}

impl FsProbe for Fs {
    fn children(&self, dir: &str) -> Option<Vec<(String, bool)>> {
        let n = self.0.find(&path_components_raw(dir))?;
        Some(
            n.children
                .values()
                .map(|c| (c.name.clone(), c.is_dir))
                .collect(),
        )
    }

    fn entry(&self, path: &str) -> Option<ProbeEntry> {
        let n = self.0.find(&path_components_raw(path))?;
        let e = n.entry();
        Some(ProbeEntry {
            is_dir: n.is_dir,
            size: e.size,
            newest_mtime: e.newest_mtime,
            flags: e.flags,
            magic: e.magic,
        })
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

fn fixtures() -> Fixtures {
    let text = include_str!("fixtures/rules.toml");
    toml::from_str(text).expect("fixtures parse")
}

fn is_dir_path(p: &str) -> bool {
    p.ends_with('\\')
}

#[test]
fn every_builtin_rule_has_a_fixture() {
    let rules = RuleSet::builtin().unwrap();
    let fx = fixtures();
    let covered: BTreeSet<&str> = fx.cases.iter().map(|c| c.rule.as_str()).collect();
    let missing: Vec<&str> = rules
        .rules()
        .iter()
        .map(|r| r.id.as_str())
        .filter(|id| !covered.contains(id))
        .collect();
    assert!(missing.is_empty(), "rules without fixtures: {missing:#?}");
    let unknown: Vec<&str> = covered
        .iter()
        .copied()
        .filter(|id| !rules.rules().iter().any(|r| r.id == *id))
        .collect();
    assert!(
        unknown.is_empty(),
        "fixtures for unknown rules: {unknown:#?}"
    );
    for case in &fx.cases {
        assert!(!case.matches.is_empty(), "{}: no positive paths", case.rule);
        assert!(
            case.auto_near_miss || !case.no_match.is_empty(),
            "{}: needs near-miss paths",
            case.rule
        );
    }
}

#[test]
fn builtin_rule_fixtures() {
    let c = classifier();
    let kf = known_folders();
    let dynamic = dynamic_roots();
    let mut failures = Vec::new();
    let mut checked = 0usize;

    for case in fixtures().cases {
        let rid = c
            .rule_id(&case.rule)
            .unwrap_or_else(|| panic!("unknown rule {}", case.rule));
        let mut root = Node {
            is_dir: true,
            ..Default::default()
        };
        let mut add = |path: &str, is_dir: bool, attrs: Attrs| {
            root.insert(&path_components_raw(path), is_dir, attrs);
        };
        let mut specs: BTreeMap<String, (bool, Attrs)> = BTreeMap::new();
        for item in &case.tree {
            let spec = item.spec();
            for (p, _) in expand(&spec.path, &kf, &dynamic) {
                let a = attrs_of(&spec);
                specs.insert(normalize_path(&p), (is_dir_path(&p), a.clone()));
                add(&p, is_dir_path(&p), a);
            }
        }
        let mut positives = Vec::new();
        for m in &case.matches {
            for (p, u) in expand(m, &kf, &dynamic) {
                let key = normalize_path(&p);
                let (is_dir, attrs) = specs
                    .get(&key)
                    .cloned()
                    .unwrap_or((is_dir_path(&p), Attrs::default()));
                if !specs.contains_key(&key) {
                    add(&p, is_dir, attrs.clone());
                }
                if case.auto_near_miss {
                    let near = format!("{}_nearmiss", p.trim_end_matches('\\'));
                    let near_attrs = Attrs {
                        magic: None,
                        flags: EntryFlags::EMPTY,
                        ..attrs.clone()
                    };
                    add(&near, is_dir, near_attrs);
                    positives.push((near, None, false));
                }
                positives.push((p, u, true));
            }
        }
        for n in &case.no_match {
            for (p, _) in expand(n, &kf, &dynamic) {
                if !specs.contains_key(&normalize_path(&p)) {
                    add(&p, is_dir_path(&p), Attrs::default());
                }
                positives.push((p, None, false));
            }
        }
        root.finish(DEFAULT_AGE_DAYS);
        let fs = Fs(root);
        let results = fs.classify_all(&c);

        for (p, user, want) in positives {
            checked += 1;
            let key = normalize_path(&p);
            let Some(cls) = results.get(&key) else {
                failures.push(format!("{}: {p} was not classified", case.rule));
                continue;
            };
            let own = cls.rule == Some(rid) && !cls.inherited;
            if own != want {
                let e = c.explain(&p, &fs);
                failures.push(format!(
                    "{}: {p} should {}match\n{}",
                    case.rule,
                    if want { "" } else { "not " },
                    e.to_text(&c)
                ));
            } else if want && user.is_some() && cls.user != user {
                failures.push(format!(
                    "{}: {p} attributed to user {:?}, expected {user:?}",
                    case.rule, cls.user
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} fixture failures:\n\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(checked > 400, "suspiciously few assertions: {checked}");
    println!("{checked} fixture assertions");
}

#[test]
fn defaults_outside_rules() {
    let c = classifier();
    let file = Entry::file("x.dat").with_size(10);
    // Unmatched paths under Windows and Program Files are never.
    for p in [
        r"C:\Windows\System32\drivers\x.dat",
        r"C:\Program Files\Vendor\App\x.dat",
        r"C:\Program Files (x86)\x.dat",
    ] {
        let cls = c.classify_path(p, &file);
        assert_eq!(cls.safety, strata_core::Safety::Never, "{p}");
        assert!(cls.inherited, "{p}");
    }
    // Unknown elsewhere makes no claim (careful).
    let cls = c.classify_path(r"E:\random\x.dat", &file);
    assert_eq!(cls, Classification::UNKNOWN);
}

#[test]
fn no_builtin_never_rule_can_be_relaxed() {
    let rules = RuleSet::builtin().unwrap();
    let never: Vec<_> = rules
        .rules()
        .iter()
        .filter(|r| r.safety == strata_core::Safety::Never)
        .collect();
    assert!(never.len() > 20);
    let ids: Vec<String> = never.iter().map(|r| format!("\"{}\"", r.id)).collect();
    let mut text = format!(
        "schema_version = 1\npack = \"user\"\ndisable = [{}]\n",
        ids.join(",")
    );
    for r in &never {
        text.push_str(&format!(
            "[[rule]]\nid = \"{}\"\nname = \"x\"\ncategory = \"temp\"\nsafety = \"safe\"\nexplain = \"x\"\nmatch.ext = \"zzz\"\n",
            r.id
        ));
    }
    let loaded = RuleSet::load(&[strata_classify::UserPack {
        file: "user.toml".into(),
        text,
    }])
    .unwrap();
    assert_eq!(
        loaded.report().errors.len(),
        never.len() * 2,
        "{:#?}",
        loaded.report().errors
    );
    for r in &never {
        let now = loaded.rules().iter().find(|x| x.id == r.id).unwrap();
        assert_eq!(now.safety, strata_core::Safety::Never);
        assert!(now.is_builtin());
    }
}
