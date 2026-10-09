//! The compiled rule engine and its top-down tree-walk API.
//!
//! Responsibilities:
//! - Resolve rule path tokens against a [`KnownFolders`] value (every user
//!   profile) plus caller-supplied [`DynamicRoots`].
//! - Compile path rules into one component trie that runs as an NFA (glob
//!   components and `**` supported), and name/extension rules into hash maps,
//!   with glob/regex sets only for the patterns that need them.
//! - Classify entries during a top-down walk: the caller hands each directory's
//!   children to [`Classifier::enter_dir`], which returns a [`DirScope`] used
//!   to classify that directory's children. No per-entry path strings are
//!   built.
//! - Apply precedence, inheritance and the safety invariants documented in
//!   `docs/RULES.md`.
//!
//! # Precedence algorithm
//!
//! For one entry:
//! 1. Collect candidate rules from the trie (path / path_glob), name maps and
//!    sets, extension map, flag/magic/any lists and armed path regexes.
//! 2. Drop candidates whose secondary matchers fail.
//! 3. The best own match maximizes `(class, kind, specificity, safety,
//!    user-over-builtin, earlier rule)`.
//! 4. The own match replaces the inherited classification when its class is
//!    at least the inherited class, except: an own `never` match always wins
//!    over a less strict inherited tier, and an inherited `never` can only be
//!    replaced by a less strict tier through a path-class rule (an explicit
//!    carve-out such as `{WINDIR}\Temp`).
//! 5. When user rules are loaded, the same decision runs with built-in rules
//!    only; if that says `never`, the result is clamped to `never`.
//!
//! Cost per entry is one name fold, one hash lookup per active trie state,
//! one name and one extension lookup, plus glob/regex sets only when such
//! rules exist.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

use globset::{GlobBuilder, GlobMatcher};
use regex::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use serde::Serialize;
use smallvec::SmallVec;
use strata_core::known::{KnownFolder, KnownFolders};
use strata_core::{Category, CloudState, EntryFlags, FileTime, Safety, WideName};

use crate::fold::{
    extension, fold_str_into, fold_units_into, path_components, stem, strip_copy_suffix,
};
use crate::pack::RuleSet;
use crate::schema::{
    AppliesTo, Component, FlagMatch, Inherit, MatchedBy, NameKind, NameReq, PathPattern,
    PatternRoot, Rule,
};
use crate::sniff::DetectedType;

// -----------------------------------------------------------------------------
// Public input/output types
// -----------------------------------------------------------------------------

/// Index of a rule inside a [`Classifier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct RuleId(pub u32);

/// A borrowed entry name, raw UTF-16 or UTF-8.
#[derive(Debug, Clone, Copy)]
pub enum Name<'a> {
    /// Raw UTF-16 units (lossless form from the scanners).
    Wide(&'a [u16]),
    /// UTF-8 text.
    Str(&'a str),
}

impl<'a> From<&'a str> for Name<'a> {
    fn from(s: &'a str) -> Self {
        Self::Str(s)
    }
}

impl<'a> From<&'a String> for Name<'a> {
    fn from(s: &'a String) -> Self {
        Self::Str(s)
    }
}

impl<'a> From<&'a [u16]> for Name<'a> {
    fn from(s: &'a [u16]) -> Self {
        Self::Wide(s)
    }
}

impl<'a> From<&'a WideName> for Name<'a> {
    fn from(s: &'a WideName) -> Self {
        Self::Wide(s.units())
    }
}

impl Name<'_> {
    fn fold_into(self, out: &mut String) {
        out.clear();
        match self {
            Self::Wide(u) => fold_units_into(u, out),
            Self::Str(s) => fold_str_into(s, out),
        }
    }
}

/// One child of a directory, for sibling/child checks.
#[derive(Debug, Clone, Copy)]
pub struct ChildRef<'a> {
    /// Child name.
    pub name: Name<'a>,
    /// Whether the child is a directory.
    pub is_dir: bool,
}

impl<'a> ChildRef<'a> {
    /// A file child.
    #[must_use]
    pub fn file(name: impl Into<Name<'a>>) -> Self {
        Self {
            name: name.into(),
            is_dir: false,
        }
    }

    /// A directory child.
    #[must_use]
    pub fn dir(name: impl Into<Name<'a>>) -> Self {
        Self {
            name: name.into(),
            is_dir: true,
        }
    }
}

/// Facts about the entry being classified.
///
/// For directories, `size` and `newest_mtime` are subtree aggregates (allocated
/// or logical size per the caller's size mode; newest modification anywhere
/// below). Missing facts make `min_size`/`older_than_days` rules not match.
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    /// Entry name.
    pub name: Name<'a>,
    /// Entry flags; [`EntryFlags::DIR`] decides file vs directory.
    pub flags: EntryFlags,
    /// Size in bytes (subtree total for directories).
    pub size: u64,
    /// Newest modification time (subtree newest for directories).
    pub newest_mtime: Option<FileTime>,
    /// Sniffed content type, when known.
    pub magic: Option<DetectedType>,
}

impl<'a> Entry<'a> {
    /// A file entry with no size, time or sniff facts.
    #[must_use]
    pub fn file(name: impl Into<Name<'a>>) -> Self {
        Self {
            name: name.into(),
            flags: EntryFlags::EMPTY,
            size: 0,
            newest_mtime: None,
            magic: None,
        }
    }

    /// A directory entry with no size or time facts.
    #[must_use]
    pub fn dir(name: impl Into<Name<'a>>) -> Self {
        Self {
            flags: EntryFlags::DIR,
            ..Self::file(name)
        }
    }

    /// Sets the size.
    #[must_use]
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = size;
        self
    }

    /// Sets the newest modification time.
    #[must_use]
    pub fn with_mtime(mut self, t: FileTime) -> Self {
        self.newest_mtime = Some(t);
        self
    }

    /// Sets the sniffed type.
    #[must_use]
    pub fn with_magic(mut self, t: DetectedType) -> Self {
        self.magic = Some(t);
        self
    }

    /// Adds flags.
    #[must_use]
    pub fn with_flags(mut self, f: EntryFlags) -> Self {
        self.flags = self.flags.union(f);
        self
    }

    fn is_dir(&self) -> bool {
        self.flags.contains(EntryFlags::DIR)
    }
}

/// The classification of one entry, with the facts needed to explain it.
///
/// Compact and `Copy`; rule details are looked up with [`Classifier::rule`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Classification {
    /// Winning rule, or `None` for the built-in default.
    pub rule: Option<RuleId>,
    /// Category.
    pub category: Category,
    /// Safety tier.
    pub safety: Safety,
    /// Kind of matcher that selected the rule.
    pub matched_by: MatchedBy,
    /// Whether the rule matched an ancestor rather than this entry.
    pub inherited: bool,
    /// Depth of the entry the rule matched (volume root = 0). Equal to this
    /// entry's depth when not inherited.
    pub origin_depth: u16,
    /// Whether the safety was raised to `never` because built-in rules say
    /// `never` and a user rule tried to relax it.
    pub clamped: bool,
    /// Index into [`KnownFolders::users`] of the profile containing the entry.
    pub user: Option<u16>,
}

impl Classification {
    /// The default for entries no rule covers: no safety claim.
    ///
    /// `Unknown` data is treated as `careful` for deletion purposes.
    pub const UNKNOWN: Self = Self {
        rule: None,
        category: Category::Unknown,
        safety: Safety::Careful,
        matched_by: MatchedBy::Default,
        inherited: false,
        origin_depth: 0,
        clamped: false,
        user: None,
    };
}

/// A [`Classification`] packed into 32 bits for per-entry storage in the
/// index.
///
/// Layout: bits 0..16 rule index + 1 (0 = no rule), 16..20 [`Category`],
/// 20..22 [`Safety`], 22..26 [`MatchedBy`], bit 26 `inherited`, bit 27
/// `clamped`. `origin_depth` and `user` are not stored; [`Classifier::explain`]
/// recomputes them on demand.
///
/// # Example
///
/// ```
/// use strata_classify::{Classification, PackedClass};
/// let c = Classification::UNKNOWN;
/// assert_eq!(PackedClass::pack(&c).unpack(), c);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(transparent)]
pub struct PackedClass(pub u32);

const MATCHED_BY_ALL: [MatchedBy; 9] = [
    MatchedBy::Default,
    MatchedBy::Any,
    MatchedBy::Magic,
    MatchedBy::Ext,
    MatchedBy::Name,
    MatchedBy::Flag,
    MatchedBy::PathRegex,
    MatchedBy::PathGlob,
    MatchedBy::Path,
];

impl PackedClass {
    /// Packs a classification (dropping `origin_depth` and `user`).
    #[must_use]
    pub fn pack(c: &Classification) -> Self {
        let rule = c.rule.map_or(0, |r| (r.0 + 1).min(0xFFFF));
        let cat = (c.category as u32) & 0xF;
        let safety = c.safety as u32;
        let kind = MATCHED_BY_ALL
            .iter()
            .position(|k| *k == c.matched_by)
            .unwrap_or(0) as u32;
        Self(
            rule | cat << 16
                | safety << 20
                | kind << 22
                | u32::from(c.inherited) << 26
                | u32::from(c.clamped) << 27,
        )
    }

    /// Unpacks; `origin_depth` is 0 and `user` is `None`.
    #[must_use]
    pub fn unpack(self) -> Classification {
        let v = self.0;
        let rule = v & 0xFFFF;
        let safety = match (v >> 20) & 0x3 {
            0 => Safety::Safe,
            1 => Safety::Probably,
            2 => Safety::Careful,
            _ => Safety::Never,
        };
        Classification {
            rule: (rule != 0).then(|| RuleId(rule - 1)),
            category: Category::from_u16(((v >> 16) & 0xF) as u16),
            safety,
            matched_by: MATCHED_BY_ALL
                .get(((v >> 22) & 0xF) as usize)
                .copied()
                .unwrap_or(MatchedBy::Default),
            inherited: v & (1 << 26) != 0,
            origin_depth: 0,
            clamped: v & (1 << 27) != 0,
            user: None,
        }
    }
}

/// Extra token roots discovered at runtime (Steam libraries, OBS recording
/// folders, Firefox profiles in custom locations, ...).
///
/// Token names are upper snake case without braces, e.g. `STEAM_LIBRARY`.
/// Rules that reference a token with no roots simply do not match.
#[derive(Debug, Clone, Default)]
pub struct DynamicRoots {
    roots: BTreeMap<String, Vec<PathBuf>>,
}

impl DynamicRoots {
    /// Adds a root for `token` (without braces).
    pub fn insert(&mut self, token: &str, path: impl Into<PathBuf>) {
        let v = self.roots.entry(token.to_string()).or_default();
        let p = path.into();
        if !v.contains(&p) {
            v.push(p);
        }
    }

    /// Roots registered for `token`.
    #[must_use]
    pub fn get(&self, token: &str) -> &[PathBuf] {
        self.roots.get(token).map_or(&[], Vec::as_slice)
    }

    /// All tokens and their roots.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[PathBuf])> {
        self.roots.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }
}

/// Errors compiling a [`RuleSet`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    /// More than 128 distinct `under`/`not_under` roots.
    #[error("too many distinct under/not_under roots ({0}; limit 128)")]
    TooManyRoots(usize),
    /// More than 128 distinct sibling/child name requirements.
    #[error("too many distinct requires_sibling/requires_child names ({0}; limit 128)")]
    TooManyNames(usize),
    /// A glob or regex failed to compile.
    #[error("rule `{0}`: {1}")]
    Pattern(String, String),
    /// More rules than [`PackedClass`] can index.
    #[error("too many rules ({0}; limit 65534)")]
    TooManyRules(usize),
}

// -----------------------------------------------------------------------------
// Trie
// -----------------------------------------------------------------------------

const ROOT: u32 = 0;

#[derive(Debug, Default)]
struct Node {
    lit: FxHashMap<Box<str>, u32>,
    /// Child for a bare `*` component (matches any name without a glob call).
    star: Option<u32>,
    wild: Vec<(Box<str>, GlobMatcher, u32)>,
    /// Epsilon edge to the `**` node below this one.
    deep: Option<u32>,
    /// This node is a `**` node: it stays active for any further component.
    self_loop: bool,
    terminals: SmallVec<[u32; 2]>,
    marks: u128,
    user: Option<u16>,
    /// Literal path of this node when it is a regex-arming `under` root.
    arm_path: Option<Arc<str>>,
}

// PERF: inside AppData a directory typically has 3-6 active states (its own
// node, the root `**` node and `*` children); 8 keeps them off the heap.
type States = SmallVec<[u32; 8]>;

/// A glob over one (folded) name.
#[derive(Debug)]
enum NameGlob {
    /// `prefix*suffix` (either part may be empty): plain string checks.
    Affix(Box<str>, Box<str>),
    /// Anything else (`?`, classes, alternation, several `*`).
    Full(GlobMatcher),
}

impl NameGlob {
    fn new(pattern: &str) -> Result<Self, String> {
        let meta = |c: char| matches!(c, '?' | '[' | ']' | '{' | '}' | '\\');
        if pattern.matches('*').count() == 1 && !pattern.contains(meta) {
            let (pre, suf) = pattern.split_once('*').unwrap_or((pattern, ""));
            return Ok(Self::Affix(pre.into(), suf.into()));
        }
        let m = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| e.to_string())?
            .compile_matcher();
        Ok(Self::Full(m))
    }

    fn is_match(&self, name: &str) -> bool {
        match self {
            Self::Affix(p, s) => {
                name.len() >= p.len() + s.len() && name.starts_with(&**p) && name.ends_with(&**s)
            }
            Self::Full(m) => m.is_match(name),
        }
    }
}

/// Name globs and regexes, grouped by the `under` roots their rules need.
#[derive(Debug)]
struct NameSet {
    /// Roots one of which must be active for the set to be evaluated
    /// (0: always evaluated).
    mask: u128,
    globs: Vec<(NameGlob, u32, NameKind)>,
    regex: Option<(RegexSet, Vec<u32>)>,
}

#[derive(Default)]
struct NameSetBuilder {
    mask: u128,
    globs: Vec<(NameGlob, u32, NameKind)>,
    regex: Vec<String>,
    regex_owners: Vec<u32>,
}

impl NameSetBuilder {
    fn build(self) -> Result<NameSet, CompileError> {
        let globs = self.globs;
        let regex = if self.regex_owners.is_empty() {
            None
        } else {
            let set = RegexSetBuilder::new(&self.regex)
                .case_insensitive(true)
                .build()
                .map_err(|e| CompileError::Pattern("name_regex".into(), e.to_string()))?;
            Some((set, self.regex_owners))
        };
        Ok(NameSet {
            mask: self.mask,
            globs,
            regex,
        })
    }
}

/// End nodes of an inserted pattern, one per resolved root, with the literal
/// path of each end when the pattern has no wildcards.
type PatternEnds = Vec<(u32, Option<Arc<str>>)>;

// -----------------------------------------------------------------------------
// Compiled rules
// -----------------------------------------------------------------------------

#[derive(Debug, Default)]
struct NameFilter {
    lits: Vec<(Box<str>, NameKind)>,
    globs: Vec<(NameGlob, NameKind)>,
    regex: Option<RegexSet>,
}

impl NameFilter {
    fn matches(&self, name: &str, is_dir: bool) -> bool {
        self.lits
            .iter()
            .any(|(l, k)| k.allows(is_dir) && **l == *name)
            || self
                .globs
                .iter()
                .any(|(g, k)| k.allows(is_dir) && g.is_match(name))
            || self.regex.as_ref().is_some_and(|r| r.is_match(name))
    }
}

const FLAG_NTFS: u8 = 1;
const FLAG_CLOUD: u8 = 2;
const FLAG_ONLINE: u8 = 4;

fn flag_bit(f: FlagMatch) -> u8 {
    match f {
        FlagMatch::NtfsMetadata => FLAG_NTFS,
        FlagMatch::Cloud => FLAG_CLOUD,
        FlagMatch::CloudOnlineOnly => FLAG_ONLINE,
    }
}

fn entry_flag_bits(f: EntryFlags) -> u8 {
    let mut b = 0;
    if f.contains(EntryFlags::NTFS_METADATA) {
        b |= FLAG_NTFS;
    }
    match f.cloud() {
        CloudState::None => {}
        CloudState::OnlineOnly => b |= FLAG_CLOUD | FLAG_ONLINE,
        _ => b |= FLAG_CLOUD,
    }
    b
}

fn magic_bit(t: DetectedType) -> u32 {
    let idx = DetectedType::ALL.iter().position(|x| *x == t).unwrap_or(0);
    1u32 << idx
}

#[derive(Debug)]
struct CRule {
    kind: MatchedBy,
    class: i8,
    specificity: u32,
    safety: Safety,
    builtin: bool,
    category: Category,
    inherit: Inherit,
    applies: AppliesTo,
    name_filter: Option<NameFilter>,
    ext_filter: Option<Box<[Box<str>]>>,
    flag_mask: u8,
    magic_mask: u32,
    under_mask: u128,
    not_under_mask: u128,
    sib_mask: u128,
    sib_stem: bool,
    sib_original: bool,
    has_sib: bool,
    child_mask: u128,
    has_child: bool,
    min_size: Option<u64>,
    max_size: Option<u64>,
    older_than_secs: Option<i64>,
    volume_root: bool,
    empty_dir: bool,
}

#[derive(Debug, Default)]
struct Interest {
    lit: FxHashMap<Box<str>, u128>,
    ext: FxHashMap<Box<str>, u128>,
    globs: Vec<(NameGlob, u128)>,
}

impl Interest {
    fn bits(&self, folded: &str) -> u128 {
        let mut b = self.lit.get(folded).copied().unwrap_or(0);
        if !self.ext.is_empty()
            && let Some(e) = extension(folded)
        {
            b |= self.ext.get(e).copied().unwrap_or(0);
        }
        for (g, bit) in &self.globs {
            if g.is_match(folded) {
                b |= bit;
            }
        }
        b
    }
}

// -----------------------------------------------------------------------------
// Scope
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Inh {
    cls: Classification,
    class: i8,
}

impl Inh {
    const DEFAULT: Self = Self {
        cls: Classification::UNKNOWN,
        class: -1,
    };
}

/// Classification state of one directory, used to classify its children.
///
/// Created by [`Classifier::root`] and [`Classifier::enter_dir`]. Holds the
/// active trie states, the `under` roots in effect, which interesting names
/// exist among the children, and what children inherit.
#[derive(Debug, Clone)]
pub struct DirScope {
    states: States,
    roots: u128,
    child_bits: u128,
    child_names: Option<Box<FxHashSet<u64>>>,
    child_count: u32,
    depth: u16,
    user: Option<u16>,
    path: Option<Arc<str>>,
    own: Classification,
    inh: Inh,
    shadow: Option<Inh>,
}

impl DirScope {
    /// The directory's own classification.
    #[must_use]
    pub fn classification(&self) -> Classification {
        self.own
    }

    /// Depth of the directory (volume root = 0).
    #[must_use]
    pub fn depth(&self) -> u16 {
        self.depth
    }

    /// Number of children supplied when the scope was created.
    #[must_use]
    pub fn child_count(&self) -> u32 {
        self.child_count
    }
}

// -----------------------------------------------------------------------------
// Trace (for explanations)
// -----------------------------------------------------------------------------

/// One candidate rule considered for an entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateTrace {
    /// The rule.
    pub rule: RuleId,
    /// `None` if every matcher held; otherwise the first failing matcher.
    pub failed: Option<&'static str>,
}

/// Why the final classification was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// No rule matched here; the inherited classification (or the default)
    /// applies.
    NoOwnMatch,
    /// The best own match won.
    OwnMatch,
    /// An own `never` match overrode a less strict inherited tier.
    OwnNeverWins,
    /// The inherited classification has a higher precedence class.
    InheritedHigherClass,
    /// The inherited classification is `never`, and only path-class rules may
    /// carve exceptions out of it.
    InheritedNeverNotCarvable,
}

/// Detailed record of one entry's evaluation.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Trace {
    /// Every candidate considered, with the failing matcher if any.
    pub candidates: Vec<CandidateTrace>,
    /// Best own match, if any.
    pub best_own: Option<RuleId>,
    /// Decision taken.
    pub decision: Option<Decision>,
    /// What the entry would inherit without its own match.
    pub inherited: Option<Classification>,
    /// Result without user rules, when user rules are loaded.
    pub builtin_only: Option<Classification>,
}

// -----------------------------------------------------------------------------
// Classifier
// -----------------------------------------------------------------------------

/// Facts about the entry being evaluated (internal).
struct Facts<'a> {
    name: &'a str,
    is_dir: bool,
    size: u64,
    mtime: Option<i64>,
    flag_bits: u8,
    magic: Option<DetectedType>,
    child_bits: u128,
    child_count: u32,
    depth: u16,
    /// Children unknown (access denied, partial scan): never "empty".
    incomplete: bool,
}

/// The compiled classifier. Immutable and `Sync`: share it across threads
/// and classify subtrees in parallel.
///
/// # Example
///
/// ```
/// use strata_classify::{ChildRef, Classifier, Entry, RuleSet};
/// use strata_core::known::{KnownFolder, KnownFolders, UserFolders};
///
/// let mut kf = KnownFolders::default();
/// kf.machine.insert(KnownFolder::Windir, r"C:\Windows".into());
/// let mut me = UserFolders { is_current: true, ..Default::default() };
/// me.folders.insert(KnownFolder::UserProfile, r"C:\Users\me".into());
/// kf.users.push(me);
///
/// let c = Classifier::new(&RuleSet::builtin().unwrap(), &kf, &Default::default()).unwrap();
/// // Walk top-down: C:\Users\me\proj contains package.json and node_modules.
/// let proj = c.root(r"C:\Users\me\proj", [ChildRef::file("package.json"), ChildRef::dir("node_modules")]);
/// let nm = c.enter_dir(&proj, &Entry::dir("node_modules"), []);
/// let rule = c.rule(nm.classification().rule.unwrap());
/// assert_eq!(rule.id, "dev.node_modules");
/// ```
#[derive(Debug)]
pub struct Classifier {
    rules: Vec<Rule>,
    crules: Vec<CRule>,
    by_id: FxHashMap<String, RuleId>,
    nodes: Vec<Node>,
    name_lit: FxHashMap<Box<str>, SmallVec<[(u32, NameKind); 2]>>,
    /// Rules whose name pattern is `*` (no glob evaluation needed).
    name_star: Vec<(u32, NameKind)>,
    /// `[unscoped, scoped]` name glob/regex sets.
    name_sets: [NameSet; 2],
    ext: FxHashMap<Box<str>, SmallVec<[u32; 2]>>,
    flag_rules: Vec<(u32, u8)>,
    magic_rules: Vec<u32>,
    any_rules: Vec<u32>,
    regex_rules: Vec<(Regex, u32)>,
    regex_arm_mask: u128,
    interest: Interest,
    placeholder_mask: u128,
    placeholder_unscoped: bool,
    has_user_rules: bool,
    now: i64,
    unresolved: Vec<String>,
}

thread_local! {
    static NAME_BUF: RefCell<String> = RefCell::new(String::with_capacity(256));
    static CHILD_BUF: RefCell<String> = RefCell::new(String::with_capacity(256));
}

fn name_hash(folded: &str, is_dir: bool) -> u64 {
    let mut h = FxHasher::default();
    folded.hash(&mut h);
    is_dir.hash(&mut h);
    h.finish()
}

struct Builder<'k> {
    kf: &'k KnownFolders,
    dynamic: &'k DynamicRoots,
    nodes: Vec<Node>,
    root_bits: Vec<(PatternRoot, Vec<Component>)>,
    unresolved: Vec<String>,
}

impl Builder<'_> {
    fn child_lit(&mut self, n: u32, comp: &str) -> u32 {
        if let Some(&c) = self.nodes[n as usize].lit.get(comp) {
            return c;
        }
        let id = self.push();
        self.nodes[n as usize].lit.insert(comp.into(), id);
        id
    }

    fn child_wild(&mut self, n: u32, pat: &str) -> Result<u32, String> {
        if pat == "*" {
            if let Some(c) = self.nodes[n as usize].star {
                return Ok(c);
            }
            let id = self.push();
            self.nodes[n as usize].star = Some(id);
            return Ok(id);
        }
        if let Some((_, _, c)) = self.nodes[n as usize]
            .wild
            .iter()
            .find(|(p, _, _)| **p == *pat)
        {
            return Ok(*c);
        }
        let m = GlobBuilder::new(pat)
            .literal_separator(true)
            .build()
            .map_err(|e| e.to_string())?
            .compile_matcher();
        let id = self.push();
        self.nodes[n as usize].wild.push((pat.into(), m, id));
        Ok(id)
    }

    fn deep_of(&mut self, n: u32) -> u32 {
        if let Some(d) = self.nodes[n as usize].deep {
            return d;
        }
        let id = self.push();
        self.nodes[id as usize].self_loop = true;
        self.nodes[n as usize].deep = Some(id);
        id
    }

    fn push(&mut self) -> u32 {
        let id = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
        self.nodes.push(Node::default());
        id
    }

    /// Resolves a pattern root to trie start nodes, with the owning user.
    fn starts(
        &mut self,
        root: &PatternRoot,
        rule: &str,
    ) -> Vec<(u32, Option<u16>, Option<String>)> {
        let paths: Vec<(String, Option<u16>)> = match root {
            PatternRoot::Anywhere => return vec![(self.deep_of(ROOT), None, None)],
            PatternRoot::Absolute(r) => vec![(r.clone(), None)],
            PatternRoot::Token(t) if t == "SYSTEMDRIVE" => self
                .kf
                .machine
                .get(&KnownFolder::Windir)
                .and_then(|p| path_components(&p.to_string_lossy()).into_iter().next())
                .map(|drive| vec![(drive, None)])
                .unwrap_or_default(),
            PatternRoot::Token(t) => {
                if let Some(f) = KnownFolder::from_token(&format!("{{{t}}}")) {
                    self.kf
                        .resolve_all(f)
                        .map(|(u, p)| {
                            let idx = u.and_then(|u| {
                                self.kf
                                    .users
                                    .iter()
                                    .position(|x| std::ptr::eq(x, u))
                                    .and_then(|i| u16::try_from(i).ok())
                            });
                            (p.to_string_lossy().into_owned(), idx)
                        })
                        .collect()
                } else {
                    self.dynamic
                        .get(t)
                        .iter()
                        .map(|p| (p.to_string_lossy().into_owned(), None))
                        .collect()
                }
            }
        };
        if paths.is_empty()
            && let PatternRoot::Token(t) = root
        {
            let msg = format!("{{{t}}} (rule `{rule}`)");
            if !self.unresolved.contains(&msg) {
                self.unresolved.push(msg);
            }
        }
        paths
            .into_iter()
            .map(|(p, u)| {
                let comps = path_components(&p);
                let mut cur = ROOT;
                for c in &comps {
                    cur = self.child_lit(cur, c);
                }
                let joined = crate::fold::join_components(&comps);
                (cur, u, Some(joined))
            })
            .collect()
    }

    /// Inserts a pattern; returns end nodes (one per resolved root) and the
    /// literal path of each end when the pattern is literal.
    fn insert(&mut self, pat: &PathPattern, rule: &str) -> Result<PatternEnds, String> {
        let mut ends = Vec::new();
        for (start, _, base) in self.starts(&pat.root, rule) {
            let mut cur = start;
            let mut lit_path = base;
            for c in &pat.components {
                cur = match c {
                    Component::Lit(s) => {
                        if let Some(p) = lit_path.as_mut() {
                            if !p.ends_with('\\') {
                                p.push('\\');
                            }
                            p.push_str(s);
                        }
                        self.child_lit(cur, s)
                    }
                    Component::Glob(g) => {
                        lit_path = None;
                        self.child_wild(cur, g)?
                    }
                    Component::Deep => {
                        lit_path = None;
                        self.deep_of(cur)
                    }
                };
            }
            ends.push((cur, lit_path.map(Arc::from)));
        }
        Ok(ends)
    }

    fn root_bit(&mut self, pat: &PathPattern) -> Result<u128, usize> {
        let key = (pat.root.clone(), pat.components.clone());
        let idx = match self.root_bits.iter().position(|k| *k == key) {
            Some(i) => i,
            None => {
                self.root_bits.push(key);
                self.root_bits.len() - 1
            }
        };
        if idx >= 128 {
            return Err(self.root_bits.len());
        }
        Ok(1u128 << idx)
    }
}

impl Classifier {
    /// Compiles a rule set for a machine's known folders.
    ///
    /// # Errors
    ///
    /// Fails when the rule set exceeds the engine's limits (128 distinct
    /// `under` roots or 128 distinct sibling/child names) or a pattern does not
    /// compile.
    pub fn new(
        rules: &RuleSet,
        kf: &KnownFolders,
        dynamic: &DynamicRoots,
    ) -> Result<Self, CompileError> {
        if rules.rules().len() > 0xFFFE {
            return Err(CompileError::TooManyRules(rules.rules().len()));
        }
        let mut b = Builder {
            kf,
            dynamic,
            nodes: vec![Node::default()],
            root_bits: Vec::new(),
            unresolved: Vec::new(),
        };

        // Mark every profile's folders so entries carry their owning user.
        for (ui, u) in kf.users.iter().enumerate() {
            let Ok(ui) = u16::try_from(ui) else { break };
            for p in u.folders.values() {
                let mut cur = ROOT;
                for c in path_components(&p.to_string_lossy()) {
                    cur = b.child_lit(cur, &c);
                }
                b.nodes[cur as usize].user = Some(ui);
            }
        }

        let mut interest_ids: Vec<NameReq> = Vec::new();
        let mut interest_bit = |req: &NameReq| -> Result<u128, CompileError> {
            let idx = match interest_ids.iter().position(|r| r == req) {
                Some(i) => i,
                None => {
                    interest_ids.push(req.clone());
                    interest_ids.len() - 1
                }
            };
            if idx >= 128 {
                return Err(CompileError::TooManyNames(interest_ids.len()));
            }
            Ok(1u128 << idx)
        };

        let mut crules = Vec::with_capacity(rules.rules().len());
        let mut name_lit: FxHashMap<Box<str>, SmallVec<[(u32, NameKind); 2]>> =
            FxHashMap::default();
        let mut name_sets: [NameSetBuilder; 2] = Default::default();
        let mut name_star = Vec::new();
        let mut ext: FxHashMap<Box<str>, SmallVec<[u32; 2]>> = FxHashMap::default();
        let mut flag_rules = Vec::new();
        let mut magic_rules = Vec::new();
        let mut any_rules = Vec::new();
        let mut regex_rules = Vec::new();
        let mut regex_arm_mask = 0u128;
        let mut placeholder_mask = 0u128;
        let mut placeholder_unscoped = false;
        let mut by_id = FxHashMap::default();

        for (i, rule) in rules.rules().iter().enumerate() {
            let ri = u32::try_from(i).unwrap_or(u32::MAX);
            by_id.insert(rule.id.clone(), RuleId(ri));
            let m = &rule.matchers;
            let kind = m.primary();
            let pat_err = |e: String| CompileError::Pattern(rule.id.clone(), e);

            let mut under_mask = 0u128;
            for u in &m.under {
                let bit = b.root_bit(u).map_err(CompileError::TooManyRoots)?;
                under_mask |= bit;
                for (end, lit) in b.insert(u, &rule.id).map_err(pat_err)? {
                    let n = &mut b.nodes[end as usize];
                    n.marks |= bit;
                    if !m.path_regex.is_empty() && lit.is_some() {
                        n.arm_path = lit;
                    }
                }
            }
            let mut not_under_mask = 0u128;
            for u in &m.not_under {
                let bit = b.root_bit(u).map_err(CompileError::TooManyRoots)?;
                not_under_mask |= bit;
                for (end, _) in b.insert(u, &rule.id).map_err(pat_err)? {
                    b.nodes[end as usize].marks |= bit;
                }
            }

            match kind {
                MatchedBy::Path | MatchedBy::PathGlob => {
                    for p in &m.paths {
                        for (end, _) in b.insert(p, &rule.id).map_err(pat_err)? {
                            let t = &mut b.nodes[end as usize].terminals;
                            if !t.contains(&ri) {
                                t.push(ri);
                            }
                        }
                    }
                }
                MatchedBy::PathRegex => {
                    regex_arm_mask |= under_mask;
                    for r in &m.path_regex {
                        let re = RegexBuilder::new(r)
                            .case_insensitive(true)
                            .build()
                            .map_err(|e| pat_err(e.to_string()))?;
                        regex_rules.push((re, ri));
                    }
                }
                MatchedBy::Flag => {
                    flag_rules.push((ri, m.flags.iter().fold(0u8, |a, f| a | flag_bit(*f))));
                }
                MatchedBy::Name => {
                    // PERF: rules scoped by `under` only pay for glob/regex
                    // evaluation inside their roots.
                    let set = &mut name_sets[usize::from(under_mask != 0)];
                    set.mask |= under_mask;
                    for n in &m.names {
                        if n.pattern == "*" {
                            name_star.push((ri, n.kind));
                        } else if n.is_glob {
                            let g = NameGlob::new(&n.pattern).map_err(pat_err)?;
                            set.globs.push((g, ri, n.kind));
                        } else {
                            name_lit
                                .entry(n.pattern.as_str().into())
                                .or_default()
                                .push((ri, n.kind));
                        }
                    }
                    for r in &m.name_regex {
                        set.regex.push(r.clone());
                        set.regex_owners.push(ri);
                    }
                }
                MatchedBy::Ext => {
                    for e in &m.exts {
                        let v = ext.entry(e.as_str().into()).or_default();
                        if !v.contains(&ri) {
                            v.push(ri);
                        }
                    }
                }
                MatchedBy::Magic => magic_rules.push(ri),
                MatchedBy::Any => any_rules.push(ri),
                MatchedBy::Default => {}
            }

            let name_filter =
                if kind != MatchedBy::Name && (!m.names.is_empty() || !m.name_regex.is_empty()) {
                    let mut f = NameFilter::default();
                    for n in &m.names {
                        if n.is_glob {
                            f.globs
                                .push((NameGlob::new(&n.pattern).map_err(pat_err)?, n.kind));
                        } else {
                            f.lits.push((n.pattern.as_str().into(), n.kind));
                        }
                    }
                    if !m.name_regex.is_empty() {
                        f.regex = Some(
                            RegexSetBuilder::new(&m.name_regex)
                                .case_insensitive(true)
                                .build()
                                .map_err(|e| pat_err(e.to_string()))?,
                        );
                    }
                    Some(f)
                } else {
                    None
                };
            let ext_filter = (kind != MatchedBy::Ext && !m.exts.is_empty())
                .then(|| m.exts.iter().map(|e| e.as_str().into()).collect());

            let mut sib_mask = 0u128;
            let mut sib_stem = false;
            let mut sib_original = false;
            for r in &m.requires_sibling {
                match r {
                    NameReq::Stem => sib_stem = true,
                    NameReq::Original => sib_original = true,
                    _ => sib_mask |= interest_bit(r)?,
                }
            }
            if sib_stem || sib_original {
                if under_mask == 0 {
                    placeholder_unscoped = true;
                }
                placeholder_mask |= under_mask;
            }
            let mut child_mask = 0u128;
            for r in &m.requires_child {
                child_mask |= interest_bit(r)?;
            }

            let lit_count = m
                .paths
                .iter()
                .map(PathPattern::literal_count)
                .max()
                .unwrap_or(0);
            crules.push(CRule {
                kind,
                class: kind.class(),
                specificity: (u32::try_from(lit_count).unwrap_or(u32::MAX >> 16) << 16)
                    | u32::from(m.constraint_count()),
                safety: rule.safety,
                builtin: rule.is_builtin(),
                category: rule.category,
                inherit: rule.inherit,
                applies: m.applies_to,
                name_filter,
                ext_filter,
                flag_mask: if kind == MatchedBy::Flag {
                    0
                } else {
                    m.flags.iter().fold(0u8, |a, f| a | flag_bit(*f))
                },
                magic_mask: m.magic.iter().fold(0u32, |a, t| a | magic_bit(*t)),
                under_mask,
                not_under_mask,
                sib_mask,
                sib_stem,
                sib_original,
                has_sib: !m.requires_sibling.is_empty(),
                child_mask,
                has_child: !m.requires_child.is_empty(),
                min_size: m.min_size,
                max_size: m.max_size,
                older_than_secs: m.older_than_days.map(|d| i64::from(d) * 86_400),
                volume_root: m.volume_root,
                empty_dir: m.empty_dir,
            });
        }

        let mut interest = Interest::default();
        for (i, req) in interest_ids.iter().enumerate() {
            let bit = 1u128 << i;
            match req {
                NameReq::Lit(s) => *interest.lit.entry(s.as_str().into()).or_default() |= bit,
                NameReq::Glob(g) => {
                    let simple_ext = g
                        .strip_prefix("*.")
                        .filter(|e| !e.contains(['*', '?', '[', '{', '.']));
                    if let Some(e) = simple_ext {
                        *interest.ext.entry(e.into()).or_default() |= bit;
                    } else {
                        let ng =
                            NameGlob::new(g).map_err(|e| CompileError::Pattern(g.clone(), e))?;
                        interest.globs.push((ng, bit));
                    }
                }
                NameReq::Stem | NameReq::Original => {}
            }
        }

        let [unscoped, scoped] = name_sets;
        let name_sets = [unscoped.build()?, scoped.build()?];

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));

        Ok(Self {
            rules: rules.rules().to_vec(),
            crules,
            by_id,
            nodes: b.nodes,
            name_lit,
            name_star,
            name_sets,
            ext,
            flag_rules,
            magic_rules,
            any_rules,
            regex_rules,
            regex_arm_mask,
            interest,
            placeholder_mask,
            placeholder_unscoped,
            has_user_rules: rules.has_user_rules(),
            now,
            unresolved: b.unresolved,
        })
    }

    /// Overrides "now" (Unix seconds) for `older_than_days` checks; tests and
    /// snapshot replays use it.
    pub fn set_now(&mut self, unix_secs: i64) {
        self.now = unix_secs;
    }

    /// The rule behind an id.
    ///
    /// # Panics
    ///
    /// Panics if `id` did not come from this classifier.
    #[must_use]
    pub fn rule(&self, id: RuleId) -> &Rule {
        &self.rules[id.0 as usize]
    }

    /// Every compiled rule, indexable by [`RuleId`].
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Looks up a rule by its string id.
    #[must_use]
    pub fn rule_id(&self, id: &str) -> Option<RuleId> {
        self.by_id.get(id).copied()
    }

    /// Tokens that resolved to no path (rules using them cannot match).
    #[must_use]
    pub fn unresolved_tokens(&self) -> &[String] {
        &self.unresolved
    }

    // -------------------------------------------------------------------------
    // Trie stepping
    // -------------------------------------------------------------------------

    fn add_closure(&self, out: &mut States, n: u32) {
        if !out.contains(&n) {
            out.push(n);
        }
        if let Some(d) = self.nodes[n as usize].deep
            && !out.contains(&d)
        {
            out.push(d);
        }
    }

    fn start_states(&self) -> States {
        let mut s = States::new();
        self.add_closure(&mut s, ROOT);
        s
    }

    fn step(&self, states: &[u32], name: &str) -> States {
        let mut out = States::new();
        for &s in states {
            let n = &self.nodes[s as usize];
            if !n.lit.is_empty()
                && let Some(&c) = n.lit.get(name)
            {
                self.add_closure(&mut out, c);
            }
            if let Some(c) = n.star {
                self.add_closure(&mut out, c);
            }
            for (_, m, c) in &n.wild {
                if m.is_match(name) {
                    self.add_closure(&mut out, *c);
                }
            }
            if n.self_loop && !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }

    // -------------------------------------------------------------------------
    // Matching
    // -------------------------------------------------------------------------

    fn check(&self, r: &CRule, f: &Facts<'_>, parent: &DirScope) -> Option<&'static str> {
        if !r.applies.allows(f.is_dir) {
            return Some("applies_to");
        }
        if let Some(nf) = &r.name_filter
            && !nf.matches(f.name, f.is_dir)
        {
            return Some("name");
        }
        if let Some(exts) = &r.ext_filter {
            match extension(f.name) {
                Some(e) if exts.iter().any(|x| **x == *e) => {}
                _ => return Some("ext"),
            }
        }
        if r.flag_mask != 0 && f.flag_bits & r.flag_mask == 0 {
            return Some("flag");
        }
        if r.magic_mask != 0 && f.magic.is_none_or(|t| magic_bit(t) & r.magic_mask == 0) {
            return Some("magic");
        }
        if r.under_mask != 0 && parent.roots & r.under_mask == 0 {
            return Some("under");
        }
        if parent.roots & r.not_under_mask != 0 {
            return Some("not_under");
        }
        if r.volume_root && parent.depth != 0 {
            return Some("volume_root");
        }
        if r.has_sib {
            let mut ok = parent.child_bits & r.sib_mask != 0;
            if !ok
                && (r.sib_stem || r.sib_original)
                && let Some(set) = &parent.child_names
            {
                if r.sib_stem {
                    let s = stem(f.name);
                    ok = s.len() < f.name.len() && set.contains(&name_hash(s, true));
                }
                if !ok && r.sib_original {
                    ok = strip_copy_suffix(f.name)
                        .is_some_and(|o| set.contains(&name_hash(&o, f.is_dir)));
                }
            }
            if !ok {
                return Some("requires_sibling");
            }
        }
        if r.has_child && (!f.is_dir || f.child_bits & r.child_mask == 0) {
            return Some("requires_child");
        }
        if r.empty_dir && (!f.is_dir || f.child_count != 0 || f.incomplete) {
            return Some("empty_dir");
        }
        if r.min_size.is_some_and(|m| f.size < m) {
            return Some("min_size");
        }
        if r.max_size.is_some_and(|m| f.size > m) {
            return Some("max_size");
        }
        if let Some(age) = r.older_than_secs {
            match f.mtime {
                Some(t) if self.now.saturating_sub(t) >= age => {}
                _ => return Some("older_than_days"),
            }
        }
        None
    }

    fn collect_candidates(
        &self,
        parent: &DirScope,
        f: &Facts<'_>,
        next: &[u32],
        out: &mut SmallVec<[u32; 16]>,
    ) {
        let mut push = |r: u32| {
            if !out.contains(&r) {
                out.push(r);
            }
        };
        for &s in next {
            for &r in &self.nodes[s as usize].terminals {
                push(r);
            }
        }
        if let Some(v) = self.name_lit.get(f.name) {
            for &(r, k) in v {
                if k.allows(f.is_dir) {
                    push(r);
                }
            }
        }
        for &(r, k) in &self.name_star {
            if k.allows(f.is_dir) {
                push(r);
            }
        }
        for ns in &self.name_sets {
            if ns.mask != 0 && parent.roots & ns.mask == 0 {
                continue;
            }
            for (g, r, k) in &ns.globs {
                if k.allows(f.is_dir) && g.is_match(f.name) {
                    push(*r);
                }
            }
            if let Some((set, owners)) = &ns.regex
                && set.is_match(f.name)
            {
                for i in set.matches(f.name).iter() {
                    push(owners[i]);
                }
            }
        }
        if let Some(e) = extension(f.name)
            && let Some(v) = self.ext.get(e)
        {
            for &r in v {
                push(r);
            }
        }
        if f.flag_bits != 0 {
            for &(r, mask) in &self.flag_rules {
                if mask & f.flag_bits != 0 {
                    push(r);
                }
            }
        }
        if f.magic.is_some() {
            for &r in &self.magic_rules {
                push(r);
            }
        }
        for &r in &self.any_rules {
            push(r);
        }
        if parent.roots & self.regex_arm_mask != 0
            && let Some(base) = &parent.path
        {
            let full = format!("{base}\\{}", f.name);
            for (re, r) in &self.regex_rules {
                if self.crules[*r as usize].under_mask & parent.roots != 0 && re.is_match(&full) {
                    push(*r);
                }
            }
        }
    }

    fn better(&self, a: u32, b: u32) -> bool {
        let (ra, rb) = (&self.crules[a as usize], &self.crules[b as usize]);
        let ka = (ra.class, ra.kind, ra.specificity, ra.safety, !ra.builtin);
        let kb = (rb.class, rb.kind, rb.specificity, rb.safety, !rb.builtin);
        ka > kb || (ka == kb && a < b)
    }

    fn own_classification(&self, r: u32, f: &Facts<'_>, user: Option<u16>) -> Classification {
        let c = &self.crules[r as usize];
        Classification {
            rule: Some(RuleId(r)),
            category: c.category,
            safety: c.safety,
            matched_by: c.kind,
            inherited: false,
            origin_depth: f.depth,
            clamped: false,
            user,
        }
    }

    /// Decides between the best own match and the inherited classification.
    /// Returns the entry's classification, what its children inherit, and
    /// the decision taken.
    fn combine(
        &self,
        own: Option<u32>,
        inh: Inh,
        f: &Facts<'_>,
        user: Option<u16>,
    ) -> (Classification, Inh, Decision) {
        let inherited = Classification {
            inherited: inh.cls.rule.is_some(),
            user,
            ..inh.cls
        };
        let Some(o) = own else {
            return (inherited, inh, Decision::NoOwnMatch);
        };
        let c = &self.crules[o as usize];
        let decision = if c.safety == Safety::Never && inh.cls.safety < Safety::Never {
            Decision::OwnNeverWins
        } else if inh.cls.safety == Safety::Never
            && c.safety < Safety::Never
            && !c.kind.can_carve_out()
        {
            Decision::InheritedNeverNotCarvable
        } else if c.class >= inh.class {
            Decision::OwnMatch
        } else {
            Decision::InheritedHigherClass
        };
        if !matches!(decision, Decision::OwnMatch | Decision::OwnNeverWins) {
            return (inherited, inh, decision);
        }
        let mut cls = self.own_classification(o, f, user);
        if c.kind == MatchedBy::Flag {
            // Flags describe storage state, not ownership; they must never
            // make an entry less protected than its location already is.
            cls.safety = cls.safety.max(inh.cls.safety);
        }
        let next = match c.inherit {
            Inherit::Strong => Inh {
                cls,
                class: c.class,
            },
            Inherit::Weak => Inh { cls, class: -1 },
            Inherit::None => inh,
        };
        (cls, next, decision)
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate(
        &self,
        parent: &DirScope,
        f: &Facts<'_>,
        next: &[u32],
        user: Option<u16>,
        mut trace: Option<&mut Trace>,
    ) -> (Classification, Inh, Option<Inh>) {
        let mut cands: SmallVec<[u32; 16]> = SmallVec::new();
        self.collect_candidates(parent, f, next, &mut cands);
        let mut best: Option<u32> = None;
        let mut best_builtin: Option<u32> = None;
        for &r in &cands {
            let failed = self.check(&self.crules[r as usize], f, parent);
            if let Some(t) = trace.as_deref_mut() {
                t.candidates.push(CandidateTrace {
                    rule: RuleId(r),
                    failed,
                });
            }
            if failed.is_some() {
                continue;
            }
            if best.is_none_or(|b| self.better(r, b)) {
                best = Some(r);
            }
            if self.has_user_rules
                && self.crules[r as usize].builtin
                && best_builtin.is_none_or(|b| self.better(r, b))
            {
                best_builtin = Some(r);
            }
        }
        let (mut cls, mut next_inh, decision) = self.combine(best, parent.inh, f, user);
        let mut shadow_next = None;
        if self.has_user_rules {
            let shadow_parent = parent.shadow.unwrap_or(parent.inh);
            let (shadow_cls, s_next, _) = self.combine(best_builtin, shadow_parent, f, user);
            if shadow_cls.safety == Safety::Never && cls.safety < Safety::Never {
                cls.safety = Safety::Never;
                cls.clamped = true;
                next_inh.cls.safety = Safety::Never;
                next_inh.cls.clamped = true;
            }
            shadow_next = Some(s_next);
            if let Some(t) = trace.as_deref_mut() {
                t.builtin_only = Some(shadow_cls);
            }
        }
        if let Some(t) = trace {
            t.best_own = best.map(RuleId);
            t.decision = Some(decision);
            t.inherited = Some(parent.inh.cls);
        }
        (cls, next_inh, shadow_next)
    }

    fn facts<'n>(&self, name: &'n str, e: &Entry<'_>, parent: &DirScope) -> Facts<'n> {
        Facts {
            name,
            is_dir: e.is_dir(),
            size: e.size,
            mtime: e.newest_mtime.map(FileTime::to_unix_secs),
            flag_bits: entry_flag_bits(e.flags),
            magic: e.magic,
            child_bits: 0,
            child_count: 0,
            depth: parent.depth.saturating_add(1),
            incomplete: e.flags.contains(EntryFlags::ACCESS_DENIED)
                || e.flags.contains(EntryFlags::PARTIAL),
        }
    }

    // -------------------------------------------------------------------------
    // Public walk API
    // -------------------------------------------------------------------------

    /// Classifies a file in `parent`.
    ///
    /// # Example
    ///
    /// ```
    /// # use strata_classify::{ChildRef, Classifier, Entry, RuleSet};
    /// # let c = Classifier::new(&RuleSet::builtin().unwrap(), &Default::default(), &Default::default()).unwrap();
    /// let dir = c.root(r"D:\stuff", [ChildRef::file("debug.tmp")]);
    /// let cls = c.classify_file(&dir, &Entry::file("debug.tmp"));
    /// assert_eq!(c.rule(cls.rule.unwrap()).id, "generic.tmp_files");
    /// ```
    #[must_use]
    pub fn classify_file(&self, parent: &DirScope, entry: &Entry<'_>) -> Classification {
        NAME_BUF.with(|buf| {
            let mut buf = buf.borrow_mut();
            entry.name.fold_into(&mut buf);
            let f = self.facts(&buf, entry, parent);
            let next = self.step(&parent.states, f.name);
            let user = self.user_of(&next).or(parent.user);
            self.evaluate(parent, &f, &next, user, None).0
        })
    }

    /// Classifies a directory in `parent` and returns its scope, used to
    /// classify its own children. `children` lists the directory's entries
    /// (names and kinds) for `requires_child`, and for sibling checks of its
    /// children.
    ///
    /// # Example
    ///
    /// ```
    /// # use strata_classify::{ChildRef, Classifier, Entry, RuleSet};
    /// # let c = Classifier::new(&RuleSet::builtin().unwrap(), &Default::default(), &Default::default()).unwrap();
    /// let proj = c.root(r"D:\code\app", [ChildRef::file("Cargo.toml"), ChildRef::dir("target")]);
    /// let target = c.enter_dir(&proj, &Entry::dir("target"), [ChildRef::dir("debug")]);
    /// assert_eq!(c.rule(target.classification().rule.unwrap()).id, "dev.cargo_target");
    /// ```
    pub fn enter_dir<'c>(
        &self,
        parent: &DirScope,
        entry: &Entry<'_>,
        children: impl IntoIterator<Item = ChildRef<'c>>,
    ) -> DirScope {
        self.enter_dir_inner(parent, entry, children, None)
    }

    pub(crate) fn enter_dir_inner<'c>(
        &self,
        parent: &DirScope,
        entry: &Entry<'_>,
        children: impl IntoIterator<Item = ChildRef<'c>>,
        trace: Option<&mut Trace>,
    ) -> DirScope {
        NAME_BUF.with(|buf| {
            let mut buf = buf.borrow_mut();
            entry.name.fold_into(&mut buf);
            let name: &str = &buf;
            let next = self.step(&parent.states, name);
            let mut roots = parent.roots;
            let mut arm: Option<Arc<str>> = None;
            for &s in &next {
                let n = &self.nodes[s as usize];
                roots |= n.marks;
                if n.arm_path.is_some() {
                    arm.clone_from(&n.arm_path);
                }
            }
            let user = self.user_of(&next).or(parent.user);
            let need_names = self.placeholder_unscoped || roots & self.placeholder_mask != 0;
            let (child_bits, child_names, child_count) = self.scan_children(children, need_names);
            let mut f = self.facts(name, entry, parent);
            f.child_bits = child_bits;
            f.child_count = child_count;
            let (own, inh, shadow) = self.evaluate(parent, &f, &next, user, trace);
            let path = if roots & self.regex_arm_mask == 0 {
                None
            } else if let Some(p) = &parent.path {
                Some(Arc::from(format!("{p}\\{name}")))
            } else {
                arm
            };
            DirScope {
                states: next,
                roots,
                child_bits,
                child_names,
                child_count,
                depth: f.depth,
                user,
                path,
                own,
                inh,
                shadow,
            }
        })
    }

    fn user_of(&self, states: &[u32]) -> Option<u16> {
        states.iter().find_map(|&s| self.nodes[s as usize].user)
    }

    fn scan_children<'c>(
        &self,
        children: impl IntoIterator<Item = ChildRef<'c>>,
        need_names: bool,
    ) -> (u128, Option<Box<FxHashSet<u64>>>, u32) {
        CHILD_BUF.with(|cb| {
            let mut cb = cb.borrow_mut();
            let mut bits = 0u128;
            let mut count = 0u32;
            let mut names = need_names.then(|| Box::new(FxHashSet::default()));
            for c in children {
                count = count.saturating_add(1);
                c.name.fold_into(&mut cb);
                bits |= self.interest.bits(&cb);
                if let Some(set) = names.as_mut() {
                    set.insert(name_hash(&cb, c.is_dir));
                }
            }
            (bits, names, count)
        })
    }

    /// Creates the scope for a walk's starting directory (normally a volume
    /// root such as `C:\`; any absolute directory works) from its children.
    ///
    /// When starting below a volume root, ancestors are classified without
    /// sibling or size facts, so rules needing those cannot match on the
    /// ancestors themselves.
    #[must_use]
    pub fn root<'c>(
        &self,
        path: &str,
        children: impl IntoIterator<Item = ChildRef<'c>>,
    ) -> DirScope {
        self.root_inner(path, children, false)
    }

    /// [`Classifier::root`] for a directory whose children are unknown.
    fn root_partial(&self, path: &str) -> DirScope {
        self.root_inner(path, std::iter::empty(), true)
    }

    fn root_inner<'c>(
        &self,
        path: &str,
        children: impl IntoIterator<Item = ChildRef<'c>>,
        partial: bool,
    ) -> DirScope {
        let comps = path_components(path);
        let mut scope = DirScope {
            states: self.start_states(),
            roots: 0,
            child_bits: 0,
            child_names: None,
            child_count: 0,
            depth: 0,
            user: None,
            path: None,
            own: Classification::UNKNOWN,
            inh: Inh::DEFAULT,
            shadow: self.has_user_rules.then_some(Inh::DEFAULT),
        };
        let Some((first, rest)) = comps.split_first() else {
            return scope;
        };
        // The volume root itself: step into it without classifying (no rule
        // targets a bare volume).
        scope.states = self.step(&scope.states, first);
        for &s in &scope.states {
            scope.roots |= self.nodes[s as usize].marks;
        }
        scope.user = self.user_of(&scope.states);
        let mut children = Some(children);
        if rest.is_empty() {
            let need = self.placeholder_unscoped || scope.roots & self.placeholder_mask != 0;
            let (b, n, c) = self.scan_children(children.take().into_iter().flatten(), need);
            scope.child_bits = b;
            scope.child_names = n;
            scope.child_count = c;
            return scope;
        }
        for (i, comp) in rest.iter().enumerate() {
            scope = if i + 1 == rest.len() {
                let mut entry = Entry::dir(comp.as_str());
                entry.flags.set(EntryFlags::PARTIAL, partial);
                self.enter_dir(&scope, &entry, children.take().into_iter().flatten())
            } else {
                // Ancestors' children are unknown here; PARTIAL keeps them
                // from looking like empty folders.
                let entry = Entry::dir(comp.as_str()).with_flags(EntryFlags::PARTIAL);
                self.enter_dir(&scope, &entry, std::iter::empty())
            };
        }
        scope
    }

    /// Classifies one absolute path without a tree walk. Ancestors get no
    /// sibling/child facts; `entry` supplies the final entry's facts (its name
    /// is ignored; the path's last component is used).
    #[must_use]
    pub fn classify_path(&self, path: &str, entry: &Entry<'_>) -> Classification {
        let comps = path_components(path);
        let Some((last, parents)) = comps.split_last() else {
            return Classification::UNKNOWN;
        };
        if parents.is_empty() {
            return Classification::UNKNOWN;
        }
        // The parent's children are unknown, so it is marked partial too.
        let parent = self.root_partial(&crate::fold::join_components(parents));
        let mut e = Entry {
            name: Name::Str(last),
            ..*entry
        };
        if e.is_dir() {
            e.flags.set(EntryFlags::PARTIAL, true);
            self.enter_dir(&parent, &e, std::iter::empty())
                .classification()
        } else {
            self.classify_file(&parent, &e)
        }
    }

    pub(crate) fn classify_file_traced(
        &self,
        parent: &DirScope,
        entry: &Entry<'_>,
        trace: &mut Trace,
    ) -> Classification {
        NAME_BUF.with(|buf| {
            let mut buf = buf.borrow_mut();
            entry.name.fold_into(&mut buf);
            let f = self.facts(&buf, entry, parent);
            let next = self.step(&parent.states, f.name);
            let user = self.user_of(&next).or(parent.user);
            self.evaluate(parent, &f, &next, user, Some(trace)).0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::UserPack;
    use strata_core::known::UserFolders;

    fn kf() -> KnownFolders {
        let mut kf = KnownFolders::default();
        kf.machine.insert(KnownFolder::Windir, r"C:\Windows".into());
        kf.machine
            .insert(KnownFolder::ProgramFiles, r"C:\Program Files".into());
        let mut u = UserFolders {
            is_current: true,
            ..Default::default()
        };
        u.folders
            .insert(KnownFolder::UserProfile, r"C:\Users\a".into());
        u.folders.insert(
            KnownFolder::LocalAppData,
            r"C:\Users\a\AppData\Local".into(),
        );
        u.folders
            .insert(KnownFolder::Temp, r"C:\Users\a\AppData\Local\Temp".into());
        u.folders.insert(KnownFolder::Downloads, r"D:\Dl".into());
        kf.users.push(u);
        kf
    }

    const PACK: &str = r#"schema_version = 1
pack = "t"
[[rule]]
id = "t.windir"
name = "Windows"
category = "system"
safety = "never"
explain = "os"
match.path = "{WINDIR}"
[[rule]]
id = "t.wintemp"
name = "Windows temp"
category = "temp"
safety = "safe"
explain = "tmp"
match.path = "{WINDIR}\\Temp"
[[rule]]
id = "t.log"
name = "Logs"
category = "temp"
safety = "probably"
explain = "log"
match.ext = "log"
[[rule]]
id = "t.git"
name = "git"
category = "dev_build"
safety = "never"
explain = "git"
match.dir_name = ".git"
[[rule]]
id = "t.cache"
name = "cache"
category = "caches"
safety = "safe"
explain = "c"
match.path_glob = "{LOCALAPPDATA}\\App*\\**\\Cache"
[[rule]]
id = "t.tmp"
name = "tmp"
category = "temp"
safety = "safe"
explain = "t"
match.path = "{TEMP}"
[[rule]]
id = "t.dl"
name = "dl"
category = "downloads"
safety = "careful"
explain = "d"
inherit = "weak"
match.path = "{DOWNLOADS}"
[[rule]]
id = "t.installer"
name = "inst"
category = "downloads"
safety = "probably"
explain = "i"
match.ext = "exe"
match.under = "{DOWNLOADS}"
[[rule]]
id = "t.extracted"
name = "extracted"
category = "archives"
safety = "probably"
explain = "x"
match.ext = "zip"
match.requires_sibling = "{stem}"
match.under = "{DOWNLOADS}"
[[rule]]
id = "t.rx"
name = "rx"
category = "caches"
safety = "safe"
explain = "rx"
match.path_regex = '^C:\\USERS\\A\\APPDATA\\LOCAL\\RX\\V\d+$'
match.under = "{LOCALAPPDATA}\\Rx"
"#;

    fn classifier(user: &[UserPack]) -> Classifier {
        let rs = RuleSet::load_from(&[("t.toml", PACK)], user).unwrap();
        let mut c = Classifier::new(&rs, &kf(), &DynamicRoots::default()).unwrap();
        c.set_now(1_000_000_000);
        c
    }

    fn id(c: &Classifier, cls: Classification) -> Option<&str> {
        cls.rule.map(|r| c.rule(r).id.as_str())
    }

    #[test]
    fn carve_out_and_inheritance() {
        let c = classifier(&[]);
        let win = c.root(r"C:\Windows", []);
        assert_eq!(id(&c, win.classification()), Some("t.windir"));
        let sys = c.enter_dir(&win, &Entry::dir("System32"), []);
        assert_eq!(sys.classification().safety, Safety::Never);
        assert!(sys.classification().inherited);
        // Ext rule cannot relax inherited never.
        let log = c.classify_file(&sys, &Entry::file("x.log"));
        assert_eq!(id(&c, log), Some("t.windir"));
        // Path carve-out wins.
        let tmp = c.enter_dir(&win, &Entry::dir("TEMP"), []);
        assert_eq!(id(&c, tmp.classification()), Some("t.wintemp"));
        // Ext rule loses to inherited path class.
        let log = c.classify_file(&tmp, &Entry::file("a.LOG"));
        assert_eq!(id(&c, log), Some("t.wintemp"));
        assert_eq!(log.origin_depth, 2);
        // Own never beats less strict inherited.
        let git = c.enter_dir(&tmp, &Entry::dir(".git"), []);
        assert_eq!(id(&c, git.classification()), Some("t.git"));
    }

    #[test]
    fn globs_and_deep() {
        let c = classifier(&[]);
        let base = c.root(r"C:\Users\a\AppData\Local\AppX", []);
        let a = c.enter_dir(&base, &Entry::dir("x"), []);
        let cache = c.enter_dir(&a, &Entry::dir("cache"), []);
        assert_eq!(id(&c, cache.classification()), Some("t.cache"));
        let direct = c.enter_dir(&base, &Entry::dir("Cache"), []);
        assert_eq!(id(&c, direct.classification()), Some("t.cache"));
        let other = c.root(r"C:\Users\a\AppData\Local\Other", []);
        let no = c.enter_dir(&other, &Entry::dir("Cache"), []);
        assert_eq!(no.classification().rule, None);
        assert_eq!(no.classification().user, Some(0));
    }

    #[test]
    fn weak_inheritance_and_under() {
        let c = classifier(&[]);
        let dl = c.root(
            r"D:\Dl",
            [
                ChildRef::file("a.zip"),
                ChildRef::dir("a"),
                ChildRef::file("b.zip"),
            ],
        );
        assert_eq!(id(&c, dl.classification()), Some("t.dl"));
        let exe = c.classify_file(&dl, &Entry::file("setup.exe"));
        assert_eq!(id(&c, exe), Some("t.installer"));
        let other = c.classify_file(&dl, &Entry::file("notes.txt"));
        assert_eq!(id(&c, other), Some("t.dl"));
        let a = c.classify_file(&dl, &Entry::file("a.zip"));
        assert_eq!(id(&c, a), Some("t.extracted"));
        let b = c.classify_file(&dl, &Entry::file("b.zip"));
        assert_eq!(id(&c, b), Some("t.dl"));
        // `under` means strictly inside.
        let outside = c.root(r"D:\", []);
        let exe = c.classify_file(&outside, &Entry::file("setup.exe"));
        assert_eq!(exe.rule, None);
    }

    #[test]
    fn path_regex_is_armed_under_root() {
        let c = classifier(&[]);
        let rx = c.root(r"C:\Users\a\AppData\Local\Rx", []);
        let v = c.enter_dir(&rx, &Entry::dir("v12"), []);
        assert_eq!(id(&c, v.classification()), Some("t.rx"));
        let n = c.enter_dir(&rx, &Entry::dir("vx"), []);
        assert_eq!(n.classification().rule, None);
    }

    #[test]
    fn user_rules_cannot_relax_builtin_never() {
        let c = classifier(&[UserPack {
            file: "u.toml".into(),
            text: r#"schema_version = 1
pack = "u"
[[rule]]
id = "u.fonts"
name = "fonts"
category = "temp"
safety = "safe"
explain = "mine"
match.path = "{WINDIR}\\Fonts"
[[rule]]
id = "u.tmpsub"
name = "tmpsub"
category = "temp"
safety = "careful"
explain = "mine"
match.path = "{WINDIR}\\Temp\\keep"
"#
            .into(),
        }]);
        let win = c.root(r"C:\Windows", []);
        let fonts = c.enter_dir(&win, &Entry::dir("Fonts"), []);
        assert_eq!(id(&c, fonts.classification()), Some("u.fonts"));
        assert_eq!(fonts.classification().safety, Safety::Never);
        assert!(fonts.classification().clamped);
        let child = c.classify_file(&fonts, &Entry::file("a.ttf"));
        assert_eq!(child.safety, Safety::Never);
        // Under a built-in carve-out, user rules apply normally.
        let tmp = c.enter_dir(&win, &Entry::dir("Temp"), []);
        let keep = c.enter_dir(&tmp, &Entry::dir("keep"), []);
        assert_eq!(keep.classification().safety, Safety::Careful);
        assert!(!keep.classification().clamped);
    }

    #[test]
    fn packed_round_trip() {
        for (i, kind) in MATCHED_BY_ALL.iter().enumerate() {
            for safety in [
                Safety::Safe,
                Safety::Probably,
                Safety::Careful,
                Safety::Never,
            ] {
                for cat in Category::ALL {
                    let c = Classification {
                        rule: (i % 2 == 0).then_some(RuleId(u32::try_from(i * 1000).unwrap())),
                        category: cat,
                        safety,
                        matched_by: *kind,
                        inherited: i % 3 == 0,
                        origin_depth: 0,
                        clamped: i % 4 == 0,
                        user: None,
                    };
                    assert_eq!(PackedClass::pack(&c).unpack(), c);
                }
            }
        }
    }

    #[test]
    fn wide_names_and_case() {
        let c = classifier(&[]);
        let w: Vec<u16> = "windows".encode_utf16().collect();
        let root = c.root(r"\\?\c:\", [ChildRef::dir(&w[..])]);
        let win = c.enter_dir(&root, &Entry::dir(&w[..]), []);
        assert_eq!(id(&c, win.classification()), Some("t.windir"));
        assert_eq!(
            id(
                &c,
                c.classify_path(r"c:/windows/temp/x.log", &Entry::file(""))
            ),
            Some("t.wintemp")
        );
    }
}
