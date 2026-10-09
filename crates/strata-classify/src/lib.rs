//! Strata's classifier: what is this data, who owns it, and is it safe to
//! remove?
//!
//! Responsibilities:
//! - [`schema`] / [`pack`]: the TOML rule-pack format, built-in packs embedded
//!   from `rules/`, user packs that override built-ins, and the guarantee that
//!   built-in `never` rules cannot be relaxed.
//! - [`engine`]: the compiled [`Classifier`] and its top-down tree-walk API
//!   ([`Classifier::root`], [`Classifier::enter_dir`],
//!   [`Classifier::classify_file`]).
//! - [`explain`]: "Why is this classified as X?" for the settings UI.
//! - [`catalog`]: the installed-apps catalog and app attribution.
//! - [`discover`]: runtime roots for rules (Steam libraries, OBS recordings,
//!   Firefox profiles, WSL distros, Epic manifests, Ollama models).
//! - [`sniff`]: content-type detection from file headers.
//! - [`fold`]: Windows-correct case folding and path normalization.
//!
//! The rule engine never calls Win32: it takes a resolved
//! [`strata_core::known::KnownFolders`]. Only [`catalog`] and [`discover`]
//! touch the registry, AppX and the filesystem, behind `cfg(windows)` where
//! applicable.
//!
//! # Example
//!
//! ```
//! use strata_classify::{ChildRef, Classifier, Entry, RuleSet};
//! use strata_core::known::{KnownFolder, KnownFolders, UserFolders};
//!
//! let mut kf = KnownFolders::default();
//! let mut me = UserFolders { is_current: true, ..Default::default() };
//! me.folders.insert(KnownFolder::LocalAppData, r"C:\Users\me\AppData\Local".into());
//! kf.users.push(me);
//!
//! let rules = RuleSet::builtin().unwrap();
//! let c = Classifier::new(&rules, &kf, &Default::default()).unwrap();
//! let local = c.root(r"C:\Users\me\AppData\Local", [ChildRef::dir("npm-cache")]);
//! let npm = c.enter_dir(&local, &Entry::dir("npm-cache"), []);
//! let cls = npm.classification();
//! assert_eq!(c.rule(cls.rule.unwrap()).id, "dev.npm_cache");
//! assert_eq!(cls.safety, strata_core::Safety::Safe);
//! ```

mod builtin;
pub mod catalog;
pub mod discover;
pub mod engine;
pub mod explain;
pub mod fold;
pub mod pack;
pub mod schema;
pub mod sniff;

pub use engine::{
    ChildRef, Classification, Classifier, CompileError, Decision, DirScope, DynamicRoots, Entry,
    Name, PackedClass, RuleId, Trace,
};
pub use explain::{Explanation, FsProbe, ProbeEntry, StdFsProbe};
pub use pack::{LoadReport, RuleSet, UserPack, read_user_dir};
pub use schema::{Action, Inherit, MatchedBy, Rule, RuleSource, Tool};
pub use sniff::{DetectedType, sniff, sniff_with_tail};
