//! Classification and app attribution of a finished index.
//!
//! Responsibilities:
//! - [`Engine`]: the compiled rule set and the installed-apps catalog,
//!   built once at startup off the UI thread ([`Engine::load`]).
//! - [`AppTable`]: interned app labels; their 1-based position is the
//!   `owner_app` id stored in the index and served by `apps_brief`.
//! - [`classify_index`]: the top-down walk (`Classifier::root` /
//!   `enter_dir` / `classify_file`) that fills the per-entry
//!   [`PackedClass`] side table, the index `category` and `owner_app`
//!   columns, and the static part of every entry's packed color key.
//!
//! The walk runs level by level with rayon: every directory of one depth is
//! classified in parallel (the classifier is `Sync`, scopes are cloned per
//! directory), results are written back sequentially.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, RwLock};

use rayon::prelude::*;
use strata_classify::catalog::{AppCatalog, Attribution, InstalledApp};
use strata_classify::{
    ChildRef, Classification, Classifier, DirScope, Entry, MatchedBy, Name, PackedClass, RuleSet,
};
use strata_core::known::KnownFolders;
use strata_core::{Category, EntryFlags, Safety, SizeMode};
use strata_index::{EntryId, Index};

/// Deepest folder (volume root = 0) the catalog is asked about when nothing
/// above it is attributed. Install and app-data folders sit within this
/// depth (`C:\Users\<user>\AppData\Local\<Vendor>\<App>` is 6).
const ATTRIBUTION_MAX_DEPTH: u16 = 7;

/// Marks an entry the classifier has not seen (a scan preview between
/// passes, or an entry created after the pass).
pub const UNCLASSIFIED: u32 = u32::MAX;

/// Interned app display labels shared by every volume.
#[derive(Debug, Default)]
pub struct AppTable {
    inner: RwLock<(Vec<String>, HashMap<String, u32>)>,
}

impl AppTable {
    /// Id of `label` (1-based), interning it on first use.
    pub fn intern(&self, label: &str) -> u32 {
        if let Some(&id) = read(&self.inner).1.get(label) {
            return id;
        }
        let mut w = write(&self.inner);
        if let Some(&id) = w.1.get(label) {
            return id;
        }
        w.0.push(label.to_owned());
        let id = u32::try_from(w.0.len()).unwrap_or(u32::MAX);
        w.1.insert(label.to_owned(), id);
        id
    }

    /// Label of app `id`.
    #[must_use]
    pub fn name(&self, id: u32) -> Option<String> {
        let r = read(&self.inner);
        r.0.get(id.checked_sub(1)? as usize).cloned()
    }

    /// Every `(id, label)`.
    #[must_use]
    pub fn all(&self) -> Vec<(u32, String)> {
        read(&self.inner)
            .0
            .iter()
            .enumerate()
            .map(|(i, n)| (i as u32 + 1, n.clone()))
            .collect()
    }

    /// Whether any app whose label contains `needle` (lowercase) has id `id`.
    #[must_use]
    pub fn matches(&self, id: u32, needle: &str) -> bool {
        self.name(id)
            .is_some_and(|n| n.to_lowercase().contains(needle))
    }
}

/// The color-key app slot of an app id (`0` = unattributed, 1–1023).
#[must_use]
pub const fn app_slot(app: u32) -> u32 {
    if app == 0 { 0 } else { (app - 1) % 1023 + 1 }
}

/// Rule engine plus app catalog.
#[derive(Debug)]
pub struct Engine {
    /// Compiled rules.
    pub classifier: Classifier,
    /// Installed apps; filled by a background thread after the classifier is
    /// ready, since registry + AppX enumeration takes a second or two.
    pub catalog: CatalogCell,
    /// Interned attribution labels.
    pub apps: AppTable,
    /// Problems met while loading (user rule packs, catalog sources).
    pub warnings: Mutex<Vec<String>>,
}

impl Engine {
    /// Compiles the built-in rules plus the user rules found in
    /// `user_rules_dir`, for the given known folders.
    ///
    /// # Errors
    ///
    /// Only when the built-in packs fail to load or compile (a bug).
    pub fn new(kf: &KnownFolders, user_rules_dir: Option<&Path>) -> Result<Self, String> {
        let mut warnings = Vec::new();
        let user = match user_rules_dir.map(strata_classify::read_user_dir) {
            Some(Ok(packs)) => packs,
            Some(Err(e)) => {
                warnings.push(format!("user rules folder unreadable: {e}"));
                Vec::new()
            }
            None => Vec::new(),
        };
        let rules = RuleSet::load(&user).map_err(|errs| {
            errs.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        })?;
        warnings.extend(rules.report().errors.iter().map(ToString::to_string));
        #[cfg(windows)]
        let roots = {
            let found = strata_classify::discover::discover(kf);
            warnings.extend(found.warnings);
            found.roots
        };
        #[cfg(not(windows))]
        let roots = strata_classify::DynamicRoots::default();
        let classifier = Classifier::new(&rules, kf, &roots).map_err(|e| e.to_string())?;
        Ok(Self {
            classifier,
            catalog: CatalogCell::default(),
            apps: AppTable::default(),
            warnings: Mutex::new(warnings),
        })
    }

    /// Reads the machine's app catalog (registry, AppX, launcher games) and
    /// installs it. Blocks for a second or two.
    #[cfg(windows)]
    pub fn load_catalog(&self, kf: &KnownFolders) {
        let (mut apps, warnings) = AppCatalog::read_system(kf);
        let found = strata_classify::discover::discover(kf);
        apps.extend(found.games.iter().map(InstalledApp::from_game));
        self.catalog.set(AppCatalog::new(apps, kf));
        lock(&self.warnings).extend(warnings);
    }

    /// Installs a catalog built elsewhere (tests).
    pub fn set_catalog(&self, apps: Vec<InstalledApp>, kf: &KnownFolders) {
        self.catalog.set(AppCatalog::new(apps, kf));
    }

    /// Attribution of one path, with the rule label when the rule has one.
    #[must_use]
    pub fn attribute(&self, path: &str, cls: &Classification) -> Option<Attribution> {
        let label = self.rule_label(cls);
        let catalog = self.catalog.get()?;
        catalog.attribute(path, label)
    }

    fn rule_label(&self, cls: &Classification) -> Option<(&str, &str)> {
        let rule = self.classifier.rule(cls.rule?);
        rule.app.as_deref().map(|a| (rule.id.as_str(), a))
    }
}

/// The app catalog, replaced as a whole when activity evidence refines it.
#[derive(Debug, Default)]
pub struct CatalogCell(RwLock<Option<Arc<AppCatalog>>>);

impl CatalogCell {
    /// The catalog, once loaded.
    #[must_use]
    pub fn get(&self) -> Option<Arc<AppCatalog>> {
        read(&self.0).clone()
    }

    /// Installs a catalog.
    pub fn set(&self, catalog: AppCatalog) {
        *write(&self.0) = Some(Arc::new(catalog));
    }

    /// Edits a copy of the catalog and installs it (readers keep the old one
    /// until they ask again).
    pub fn update(&self, f: impl FnOnce(&mut AppCatalog)) {
        let Some(mut c) = self.get().map(|c| (*c).clone()) else {
            return;
        };
        f(&mut c);
        self.set(c);
    }
}

/// An [`Engine`] that becomes available once startup loading finishes.
#[derive(Debug, Default)]
pub struct EngineCell {
    slot: Mutex<Option<Result<Arc<Engine>, String>>>,
    ready: Condvar,
}

impl EngineCell {
    /// Publishes the engine (or the reason it failed to load).
    pub fn set(&self, engine: Result<Arc<Engine>, String>) {
        *lock(&self.slot) = Some(engine);
        self.ready.notify_all();
    }

    /// The engine, waiting for startup loading if needed.
    ///
    /// # Errors
    ///
    /// The load error.
    pub fn wait(&self) -> Result<Arc<Engine>, String> {
        let mut g = lock(&self.slot);
        loop {
            if let Some(r) = g.as_ref() {
                return r.clone();
            }
            g = self
                .ready
                .wait(g)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// The engine if it is loaded.
    #[must_use]
    pub fn get(&self) -> Option<Arc<Engine>> {
        lock(&self.slot)
            .as_ref()
            .and_then(|r| r.as_ref().ok().cloned())
    }
}

/// Output of [`classify_index`].
#[derive(Debug, Clone, Default)]
pub struct Classified {
    /// [`PackedClass`] bits per entry id, [`UNCLASSIFIED`] for dead slots.
    pub classes: Vec<u32>,
    /// Static color key bits per entry: category, safety, file-type slot and
    /// app slot (the age bucket is added at layout time).
    pub keys: Vec<u32>,
}

/// Safety code used on the wire: 0 unclassified, 1 safe, 2 probably,
/// 3 careful, 4 never.
///
/// Entries no rule covers carry the classifier's default "careful, no claim";
/// they are reported as unclassified (0) unless the default is `never`
/// (`{WINDIR}`, Program Files, NTFS metadata), which always shows.
#[must_use]
pub fn safety_code(class: u32) -> u8 {
    if class == UNCLASSIFIED {
        return 0;
    }
    let c = PackedClass(class).unpack();
    if c.rule.is_none() && c.safety != Safety::Never {
        return 0;
    }
    match c.safety {
        Safety::Safe => 1,
        Safety::Probably => 2,
        Safety::Careful => 3,
        Safety::Never => 4,
    }
}

/// Packed classification of virtual blocks ("Unaccounted", "Shadow copies").
fn virtual_block_class() -> u32 {
    PackedClass::pack(&Classification {
        category: Category::System,
        safety: Safety::Never,
        matched_by: MatchedBy::Default,
        ..Classification::UNKNOWN
    })
    .0
}

struct Item {
    id: u32,
    scope: DirScope,
    path: String,
    app: u32,
}

/// One child's result.
struct Out {
    id: u32,
    class: u32,
    app: u32,
}

/// Classifies every entry of `index` top-down from its root, whose display
/// path is `root_path` (`C:\` or a folder), writes categories and owning
/// apps into the index, and returns the per-entry side tables.
///
/// `ext_slots` maps an interned extension id to its file-type color slot
/// (see [`ext_slots`]).
pub fn classify_index(
    engine: &Engine,
    index: &mut Index,
    root_path: &str,
    ext_slots: &[u8],
) -> Classified {
    let slots = index.slot_count();
    let mut classes = vec![UNCLASSIFIED; slots];
    let mut apps = vec![0u32; slots];
    let c = &engine.classifier;
    let catalog = engine.catalog.get();
    let root = index.root();
    let rule_apps: Vec<u32> = c
        .rules()
        .iter()
        .map(|r| r.app.as_deref().map_or(0, |a| engine.apps.intern(a)))
        .collect();

    let root_children: Vec<(strata_core::WideName, bool)> = index
        .children(root)
        .map(|k| (index.name(k), index.is_dir(k)))
        .collect();
    let root_scope = c.root(
        root_path,
        root_children.iter().map(|(n, d)| ChildRef {
            name: Name::from(n.units()),
            is_dir: *d,
        }),
    );
    classes[root.index()] = PackedClass::pack(&root_scope.classification()).0;
    let mut levels: Vec<Vec<u32>> = vec![vec![root.0]];
    let mut frontier = vec![Item {
        id: root.0,
        scope: root_scope,
        path: root_path.trim_end_matches('\\').to_owned(),
        app: 0,
    }];

    while !frontier.is_empty() {
        let idx: &Index = index;
        let results: Vec<(Vec<Out>, Vec<Item>)> = frontier
            .par_iter()
            .map(|item| classify_children(engine, catalog.as_deref(), &rule_apps, idx, item))
            .collect();
        let mut next = Vec::new();
        let mut level = Vec::new();
        for (outs, items) in results {
            for o in outs {
                classes[o.id as usize] = o.class;
                apps[o.id as usize] = o.app;
            }
            level.extend(items.iter().map(|i| i.id));
            next.extend(items);
        }
        if !level.is_empty() {
            levels.push(level);
        }
        frontier = next;
    }

    let vclass = virtual_block_class();
    for (i, cls) in classes.iter_mut().enumerate() {
        let id = EntryId(i as u32);
        if !index.is_live(id) {
            continue;
        }
        let f = index.flags(id);
        if f.contains(EntryFlags::VIRTUAL) && !f.contains(EntryFlags::DIR) {
            *cls = vclass;
        }
    }

    for i in 0..slots {
        let id = EntryId(i as u32);
        if !index.is_live(id) || classes[i] == UNCLASSIFIED {
            continue;
        }
        let cat = PackedClass(classes[i]).unpack().category;
        index.set_category(id, cat as u16);
        index.set_owner_app(id, apps[i]);
    }

    let keys = build_keys(index, &classes, &apps, &levels, ext_slots);
    Classified { classes, keys }
}

fn classify_children(
    engine: &Engine,
    catalog: Option<&AppCatalog>,
    rule_apps: &[u32],
    index: &Index,
    item: &Item,
) -> (Vec<Out>, Vec<Item>) {
    let c = &engine.classifier;
    let mut outs = Vec::new();
    let mut dirs = Vec::new();
    for child in index.children(EntryId(item.id)) {
        let name = index.name(child);
        let flags = index.flags(child);
        if index.is_dir(child) {
            let grand: Vec<(strata_core::WideName, bool)> = index
                .children(child)
                .map(|g| (index.name(g), index.is_dir(g)))
                .collect();
            let mut entry = Entry::dir(Name::from(name.units()))
                .with_flags(flags)
                .with_size(index.size(child, SizeMode::Allocated));
            if let Some(t) = index.aggregate(child).and_then(|a| a.newest) {
                entry = entry.with_mtime(t.to_filetime());
            }
            let scope = c.enter_dir(
                &item.scope,
                &entry,
                grand.iter().map(|(n, d)| ChildRef {
                    name: Name::from(n.units()),
                    is_dir: *d,
                }),
            );
            let cls = scope.classification();
            let path = format!("{}\\{}", item.path, name.to_string_lossy());
            let label = engine.rule_label(&cls);
            let rule_app = cls.rule.map_or(0, |r| rule_apps[r.0 as usize]);
            // PERF: the catalog lookup (ancestor maps plus fuzzy folder
            // names) costs tens of microseconds. It runs where an answer
            // can appear: where a rule starts applying, and for unattributed
            // folders near the top of the tree, where install and app-data
            // folders live. Everything below inherits.
            let ask = if cls.inherited {
                item.app == 0 && scope.depth() <= ATTRIBUTION_MAX_DEPTH
            } else {
                label.is_some() || (item.app == 0 && scope.depth() <= ATTRIBUTION_MAX_DEPTH)
            };
            let app = if !ask {
                if item.app != 0 { item.app } else { rule_app }
            } else if let Some(a) = catalog.and_then(|cat| cat.attribute(&path, label)) {
                engine.apps.intern(&a.label)
            } else if rule_app != 0 {
                rule_app
            } else {
                item.app
            };
            outs.push(Out {
                id: child.0,
                class: PackedClass::pack(&cls).0,
                app,
            });
            dirs.push(Item {
                id: child.0,
                scope,
                path,
                app,
            });
        } else {
            let mut entry = Entry::file(Name::from(name.units()))
                .with_flags(flags)
                .with_size(index.own_logical(child));
            if let Some(t) = index.times(child).filter(|t| t.modified.0 != 0) {
                entry = entry.with_mtime(t.modified.to_filetime());
            }
            let cls = c.classify_file(&item.scope, &entry);
            let app = match cls.rule.map_or(0, |r| rule_apps[r.0 as usize]) {
                0 => item.app,
                a => a,
            };
            outs.push(Out {
                id: child.0,
                class: PackedClass::pack(&cls).0,
                app,
            });
        }
    }
    (outs, dirs)
}

/// File-type color slots: the 254 extensions with the most allocated bytes
/// get slots 1–254 in that order, every other extension shares 255, and
/// "no extension" is 0.
#[must_use]
pub fn ext_slots(index: &Index) -> Vec<u8> {
    let mut by_bytes = index.extension_breakdown(None, SizeMode::Allocated);
    by_bytes.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.id.cmp(&b.id)));
    let max = by_bytes.iter().map(|b| b.id).max().unwrap_or(0) as usize;
    let mut slots = vec![255u8; max + 1];
    if let Some(s) = slots.first_mut() {
        *s = 0;
    }
    for (rank, b) in by_bytes.iter().filter(|b| b.id != 0).take(254).enumerate() {
        slots[b.id as usize] = rank as u8 + 1;
    }
    slots
}

/// Static color key bits (everything but the age bucket and the live
/// "changed recently" bit). Directories use their dominant category: their
/// own rule's category, else that of their largest child.
fn build_keys(
    index: &Index,
    classes: &[u32],
    apps: &[u32],
    levels: &[Vec<u32>],
    ext_slots: &[u8],
) -> Vec<u32> {
    let mut cat = vec![0u8; classes.len()];
    for (i, &cls) in classes.iter().enumerate() {
        if cls != UNCLASSIFIED {
            cat[i] = PackedClass(cls).unpack().category as u8;
        }
    }
    for level in levels.iter().rev() {
        for &d in level {
            if cat[d as usize] != Category::Unknown as u8 {
                continue;
            }
            let largest = index
                .children(EntryId(d))
                .max_by_key(|&k| (index.size(k, SizeMode::Allocated), std::cmp::Reverse(k.0)));
            if let Some(k) = largest {
                cat[d as usize] = cat[k.index()];
            }
        }
    }
    (0..classes.len())
        .map(|i| {
            let id = EntryId(i as u32);
            if !index.is_live(id) {
                return 0;
            }
            let ty = if index.is_dir(id) {
                0
            } else {
                u32::from(
                    ext_slots
                        .get(index.ext_id(id) as usize)
                        .copied()
                        .unwrap_or(255),
                )
            };
            u32::from(cat[i] & 0xF)
                | u32::from(safety_code(classes[i])) << 4
                | ty << 12
                | app_slot(apps[i]) << 20
        })
        .collect()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_slots_wrap_into_10_bits() {
        assert_eq!(app_slot(0), 0);
        assert_eq!(app_slot(1), 1);
        assert_eq!(app_slot(1023), 1023);
        assert_eq!(app_slot(1024), 1);
    }

    #[test]
    fn app_table_interns_once() {
        let t = AppTable::default();
        assert_eq!(t.intern("Node.js"), 1);
        assert_eq!(t.intern("Steam"), 2);
        assert_eq!(t.intern("Node.js"), 1);
        assert_eq!(t.name(2).as_deref(), Some("Steam"));
        assert!(t.matches(1, "node"));
        assert_eq!(t.name(0), None);
    }

    #[test]
    fn unclaimed_default_is_unclassified_but_never_shows() {
        assert_eq!(safety_code(UNCLASSIFIED), 0);
        assert_eq!(
            safety_code(PackedClass::pack(&Classification::UNKNOWN).0),
            0
        );
        assert_eq!(safety_code(virtual_block_class()), 4);
    }
}
