//! Rule-pack schema: the TOML format and its validated form.
//!
//! Responsibilities:
//! - Deserialize pack files ([`PackFile`]) with unknown fields rejected, so a
//!   typo in a rule is an error instead of a silently ignored matcher.
//! - Expand `@list` references against pack-level `[lists]`.
//! - Validate every rule into a [`Rule`]: ids, categories, safety/action
//!   combinations, path patterns and tokens, globs, regexes and sizes.
//!
//! The full reference is `docs/RULES.md`.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use strata_core::{Category, Safety};

use crate::fold::{fold_name, fold_str_into};
use crate::sniff::DetectedType;

/// The only schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

// -----------------------------------------------------------------------------
// Raw TOML form
// -----------------------------------------------------------------------------

/// A rule pack file as written in TOML.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackFile {
    /// Schema version; must equal [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Pack id, e.g. `browsers`. Built-in rule ids start with `<pack>.`.
    pub pack: String,
    /// Pack revision, bumped when its rules change.
    #[serde(default)]
    pub version: u32,
    /// Free-form description.
    #[serde(default)]
    pub description: String,
    /// Named string lists, referenced from any list matcher as `"@name"`.
    #[serde(default)]
    pub lists: BTreeMap<String, Vec<String>>,
    /// Built-in rule ids to disable (user packs only).
    #[serde(default)]
    pub disable: Vec<String>,
    /// Rules.
    #[serde(default, rename = "rule")]
    pub rules: Vec<RawRule>,
}

/// One string or a list of strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    /// A single value.
    One(String),
    /// Several values.
    Many(Vec<String>),
}

impl OneOrMany {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s],
            Self::Many(v) => v,
        }
    }
}

/// A size given as bytes or as text with a unit (`"10 MB"`, `"1.5 GiB"`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawSize {
    /// Plain byte count.
    Bytes(u64),
    /// Number with a unit suffix.
    Text(String),
}

/// A rule as written in TOML.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRule {
    /// Namespaced unique id.
    pub id: String,
    /// Short display name.
    pub name: String,
    /// Top-level category (snake_case [`Category`] name).
    pub category: String,
    /// Optional free-form sub-category, e.g. `dev.dependencies`.
    #[serde(default)]
    pub subcategory: Option<String>,
    /// Safety tier.
    pub safety: Safety,
    /// Whether the data is regenerated automatically when deleted.
    #[serde(default)]
    pub regenerable: bool,
    /// User-facing explanation.
    pub explain: String,
    /// Action the UI offers. Defaults to `delete`, or `info_only` for `never`.
    #[serde(default)]
    pub action: Option<Action>,
    /// Tool for `open_tool` (or guidance for `info_only`).
    #[serde(default)]
    pub tool: Option<Tool>,
    /// Attribution label.
    #[serde(default)]
    pub app: Option<String>,
    /// How descendants inherit this classification.
    #[serde(default)]
    pub inherit: Option<Inherit>,
    /// Matchers.
    #[serde(rename = "match")]
    pub matchers: RawMatch,
}

/// Matchers as written in TOML. All present matchers must hold.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMatch {
    /// Exact paths (token-rooted or absolute).
    pub path: Option<OneOrMany>,
    /// Path globs, matched component by component.
    pub path_glob: Option<OneOrMany>,
    /// Regexes over the full normalized path. Requires `under`.
    pub path_regex: Option<OneOrMany>,
    /// Names of files or directories (globs allowed).
    pub name: Option<OneOrMany>,
    /// Directory names (globs allowed).
    pub dir_name: Option<OneOrMany>,
    /// File names (globs allowed).
    pub file_name: Option<OneOrMany>,
    /// Regexes over the name.
    pub name_regex: Option<OneOrMany>,
    /// File extensions, without the dot.
    pub ext: Option<OneOrMany>,
    /// Entry flags (`ntfs_metadata`, `cloud`, `cloud_online_only`).
    pub flag: Option<OneOrMany>,
    /// Matches every entry, subject to the other matchers.
    #[serde(default)]
    pub any: bool,
    /// Any of these names must exist beside the entry.
    pub requires_sibling: Option<OneOrMany>,
    /// Any of these names must exist inside the (directory) entry.
    pub requires_child: Option<OneOrMany>,
    /// Minimum size (subtree size for directories).
    pub min_size: Option<RawSize>,
    /// Maximum size (subtree size for directories).
    pub max_size: Option<RawSize>,
    /// Newest modification in the subtree is older than this many days.
    pub older_than_days: Option<u32>,
    /// Sniffed content type(s).
    pub magic: Option<OneOrMany>,
    /// Entry kinds the rule applies to.
    pub applies_to: Option<AppliesTo>,
    /// Entry must be inside one of these roots.
    pub under: Option<OneOrMany>,
    /// Entry must not be inside any of these roots.
    pub not_under: Option<OneOrMany>,
    /// Entry must sit directly in a volume root.
    #[serde(default)]
    pub volume_root: bool,
    /// Directory must have no children.
    #[serde(default)]
    pub empty_dir: bool,
}

// -----------------------------------------------------------------------------
// Validated form
// -----------------------------------------------------------------------------

/// What the UI offers for a classified entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Deletion (Recycle Bin by default) is offered.
    Delete,
    /// An official tool is offered instead of deletion.
    OpenTool,
    /// Information only.
    InfoOnly,
}

/// Official tools and guidance a rule can point to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    /// Windows Disk Cleanup (`cleanmgr`).
    DiskCleanup,
    /// `DISM /Online /Cleanup-Image /StartComponentCleanup`.
    DismComponentCleanup,
    /// `SHEmptyRecycleBinW`.
    EmptyRecycleBin,
    /// The app's registered `UninstallString`.
    AppUninstaller,
    /// Uninstall through the game launcher.
    LauncherUninstall,
    /// System Protection settings (restore points / shadow copies).
    SystemProtection,
    /// `powercfg /h off` guidance (user-run).
    HibernationGuidance,
    /// `docker system prune` guidance.
    DockerPrune,
    /// WSL virtual disk compaction guidance.
    WslCompact,
}

/// How a rule's classification flows to descendants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Inherit {
    /// Descendants inherit; only path-class rules (or `never` rules) below can
    /// override it.
    #[default]
    Strong,
    /// Descendants inherit as a fallback; any rule match below overrides it.
    Weak,
    /// Descendants do not inherit this rule (they see what this entry
    /// inherited).
    None,
}

/// Which entry kinds a rule applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppliesTo {
    /// Files only.
    File,
    /// Directories only.
    Dir,
    /// Both.
    #[default]
    Both,
}

impl AppliesTo {
    pub(crate) fn allows(self, is_dir: bool) -> bool {
        match self {
            Self::File => !is_dir,
            Self::Dir => is_dir,
            Self::Both => true,
        }
    }
}

/// Entry flags a rule can match on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagMatch {
    /// NTFS metadata file (`$MFT`, ...).
    NtfsMetadata,
    /// Any cloud-files placeholder.
    Cloud,
    /// Online-only placeholder (content not on disk).
    CloudOnlineOnly,
}

impl FlagMatch {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ntfs_metadata" => Self::NtfsMetadata,
            "cloud" => Self::Cloud,
            "cloud_online_only" => Self::CloudOnlineOnly,
            _ => return None,
        })
    }
}

/// Where a rule came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "file")]
pub enum RuleSource {
    /// Embedded built-in pack.
    Builtin,
    /// A user rules file.
    User(String),
}

/// One component of a path pattern (already case-folded).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum Component {
    /// Literal name.
    Lit(String),
    /// Glob over one component (`*`, `?`, `[..]`, `{a,b}`).
    Glob(String),
    /// `**`: zero or more components.
    Deep,
}

/// Root of a path pattern.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum PatternRoot {
    /// A token such as `{LOCALAPPDATA}` (stored without braces).
    Token(String),
    /// An absolute path's root component (`C:` or `\\SERVER\SHARE`).
    Absolute(String),
    /// Leading `**`: anywhere on any volume.
    Anywhere,
}

/// A parsed path pattern.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct PathPattern {
    /// Original text, for display.
    pub text: String,
    /// Root.
    pub root: PatternRoot,
    /// Components after the root.
    pub components: Vec<Component>,
}

impl PathPattern {
    /// Number of literal components (higher is more specific).
    #[must_use]
    pub fn literal_count(&self) -> usize {
        self.components
            .iter()
            .filter(|c| matches!(c, Component::Lit(_)))
            .count()
            + usize::from(!matches!(self.root, PatternRoot::Anywhere))
    }

    /// Whether the pattern contains no wildcard components.
    #[must_use]
    pub fn is_literal(&self) -> bool {
        self.components
            .iter()
            .all(|c| matches!(c, Component::Lit(_)))
            && !matches!(self.root, PatternRoot::Anywhere)
    }
}

/// A name requirement for `requires_sibling` / `requires_child`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum NameReq {
    /// Literal folded name.
    Lit(String),
    /// Folded glob.
    Glob(String),
    /// `{stem}`: a directory named like this entry without its extension.
    Stem,
    /// `{original}`: a file named like this entry without its ` (N)` suffix.
    Original,
}

/// Which entry kind a name pattern applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum NameKind {
    /// From `name`: files and directories.
    Any,
    /// From `dir_name`.
    Dir,
    /// From `file_name`.
    File,
}

impl NameKind {
    pub(crate) fn allows(self, is_dir: bool) -> bool {
        match self {
            Self::Any => true,
            Self::Dir => is_dir,
            Self::File => !is_dir,
        }
    }
}

/// A name pattern (folded).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct NamePattern {
    /// Folded literal or glob text.
    pub pattern: String,
    /// Whether `pattern` is a glob.
    pub is_glob: bool,
    /// Entry kinds it applies to.
    pub kind: NameKind,
}

/// The primary matcher, which selects the index a rule lives in and its
/// precedence class. See `docs/RULES.md` § Precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchedBy {
    /// No rule matched; built-in default.
    Default,
    /// `match.any`.
    Any,
    /// `match.magic` alone.
    Magic,
    /// `match.ext`.
    Ext,
    /// `match.name` / `dir_name` / `file_name` / `name_regex`.
    Name,
    /// `match.flag`.
    Flag,
    /// `match.path_regex`.
    PathRegex,
    /// `match.path_glob`.
    PathGlob,
    /// `match.path`.
    Path,
}

impl MatchedBy {
    /// Precedence class: path-like (3) > name (2) > extension/magic (1) >
    /// any (0) > default (-1).
    #[must_use]
    pub const fn class(self) -> i8 {
        match self {
            Self::Default => -1,
            Self::Any => 0,
            Self::Magic | Self::Ext => 1,
            Self::Name => 2,
            Self::Flag | Self::PathRegex | Self::PathGlob | Self::Path => 3,
        }
    }

    /// Whether a match of this kind may carve a less strict tier out of an
    /// inherited `never` classification. Only explicit path rules can.
    #[must_use]
    pub const fn can_carve_out(self) -> bool {
        matches!(self, Self::Path | Self::PathGlob | Self::PathRegex)
    }
}

/// Validated matchers of a rule.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Matchers {
    /// Exact or glob path patterns (see `path_is_glob`).
    pub paths: Vec<PathPattern>,
    /// Whether `paths` came from `path_glob`.
    pub path_is_glob: bool,
    /// Path regexes (source text; compiled case-insensitively).
    pub path_regex: Vec<String>,
    /// Name patterns.
    pub names: Vec<NamePattern>,
    /// Name regexes (source text; compiled case-insensitively).
    pub name_regex: Vec<String>,
    /// Folded extensions.
    pub exts: Vec<String>,
    /// Flags (any of).
    pub flags: Vec<FlagMatch>,
    /// `match.any`.
    pub any: bool,
    /// Sibling requirements (any of).
    pub requires_sibling: Vec<NameReq>,
    /// Child requirements (any of).
    pub requires_child: Vec<NameReq>,
    /// Minimum size in bytes.
    pub min_size: Option<u64>,
    /// Maximum size in bytes.
    pub max_size: Option<u64>,
    /// Staleness threshold in days.
    pub older_than_days: Option<u32>,
    /// Sniffed types (any of).
    pub magic: Vec<DetectedType>,
    /// Entry kinds.
    pub applies_to: AppliesTo,
    /// Required roots (any of).
    pub under: Vec<PathPattern>,
    /// Excluded roots.
    pub not_under: Vec<PathPattern>,
    /// Entry directly in a volume root.
    pub volume_root: bool,
    /// Directory without children.
    pub empty_dir: bool,
}

impl Matchers {
    /// The primary matcher kind.
    #[must_use]
    pub fn primary(&self) -> MatchedBy {
        if !self.paths.is_empty() {
            if self.path_is_glob {
                MatchedBy::PathGlob
            } else {
                MatchedBy::Path
            }
        } else if !self.path_regex.is_empty() {
            MatchedBy::PathRegex
        } else if !self.flags.is_empty() {
            MatchedBy::Flag
        } else if !self.names.is_empty() || !self.name_regex.is_empty() {
            MatchedBy::Name
        } else if !self.exts.is_empty() {
            MatchedBy::Ext
        } else if !self.magic.is_empty() {
            MatchedBy::Magic
        } else {
            MatchedBy::Any
        }
    }

    /// Number of secondary constraints; breaks ties between rules of the same
    /// kind (more constrained = more specific).
    #[must_use]
    pub fn constraint_count(&self) -> u16 {
        let p = self.primary();
        let mut n = 0u16;
        n += u16::from(!self.names.is_empty() && p != MatchedBy::Name);
        n += u16::from(!self.name_regex.is_empty() && p != MatchedBy::Name);
        n += u16::from(!self.exts.is_empty() && p != MatchedBy::Ext);
        n += u16::from(!self.magic.is_empty() && p != MatchedBy::Magic);
        n += u16::from(!self.flags.is_empty() && p != MatchedBy::Flag);
        n += u16::from(!self.requires_sibling.is_empty());
        n += u16::from(!self.requires_child.is_empty());
        n += u16::from(self.min_size.is_some());
        n += u16::from(self.max_size.is_some());
        n += u16::from(self.older_than_days.is_some());
        n += u16::from(!self.under.is_empty());
        n += u16::from(!self.not_under.is_empty());
        n += u16::from(self.volume_root);
        n += u16::from(self.empty_dir);
        n += u16::from(self.applies_to != AppliesTo::Both);
        n
    }
}

/// A validated rule.
#[derive(Debug, Clone, Serialize)]
pub struct Rule {
    /// Namespaced unique id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Pack id.
    pub pack: String,
    /// Built-in or user file.
    pub source: RuleSource,
    /// Top-level category.
    pub category: Category,
    /// Free-form sub-category.
    pub subcategory: Option<String>,
    /// Safety tier.
    pub safety: Safety,
    /// Regenerated automatically after deletion.
    pub regenerable: bool,
    /// User-facing explanation.
    pub explain: String,
    /// Offered action.
    pub action: Action,
    /// Offered tool or guidance.
    pub tool: Option<Tool>,
    /// Attribution label.
    pub app: Option<String>,
    /// Inheritance mode.
    pub inherit: Inherit,
    /// Matchers.
    pub matchers: Matchers,
}

impl Rule {
    /// Whether the rule comes from a built-in pack.
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        self.source == RuleSource::Builtin
    }
}

// -----------------------------------------------------------------------------
// Errors
// -----------------------------------------------------------------------------

/// A problem with one rule or pack.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    /// TOML syntax or shape error.
    #[error("{file}: invalid TOML: {message}")]
    Toml {
        /// Pack file name.
        file: String,
        /// Parser message.
        message: String,
    },
    /// Unsupported `schema_version`.
    #[error("{file}: unsupported schema_version {found} (expected {SCHEMA_VERSION})")]
    Version {
        /// Pack file name.
        file: String,
        /// Version found.
        found: u32,
    },
    /// A rule failed validation.
    #[error("{file}: rule `{id}`: {message}")]
    Rule {
        /// Pack file name.
        file: String,
        /// Rule id.
        id: String,
        /// What is wrong.
        message: String,
    },
    /// A pack-level problem (lists, duplicate ids, overrides).
    #[error("{file}: {message}")]
    Pack {
        /// Pack file name.
        file: String,
        /// What is wrong.
        message: String,
    },
}

// -----------------------------------------------------------------------------
// Parsing helpers
// -----------------------------------------------------------------------------

/// Parses a pack file's TOML text.
///
/// # Errors
///
/// Returns [`SchemaError::Toml`] or [`SchemaError::Version`].
pub fn parse_pack(file: &str, text: &str) -> Result<PackFile, SchemaError> {
    let pack: PackFile = toml::from_str(text).map_err(|e| SchemaError::Toml {
        file: file.to_string(),
        message: e.to_string(),
    })?;
    if pack.schema_version != SCHEMA_VERSION {
        return Err(SchemaError::Version {
            file: file.to_string(),
            found: pack.schema_version,
        });
    }
    Ok(pack)
}

/// Parses a size: plain bytes or a number with `B`, `KB`/`KiB`, `MB`/`MiB`,
/// `GB`/`GiB`, `TB`/`TiB` (decimal units are powers of 1000, binary units
/// powers of 1024).
///
/// # Errors
///
/// Returns a message for malformed or overflowing sizes.
///
/// # Example
///
/// ```
/// use strata_classify::schema::parse_size;
/// assert_eq!(parse_size("10 MB"), Ok(10_000_000));
/// assert_eq!(parse_size("1.5 GiB"), Ok(1_610_612_736));
/// assert_eq!(parse_size("512"), Ok(512));
/// ```
pub fn parse_size(text: &str) -> Result<u64, String> {
    let t = text.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let num: f64 = num
        .parse()
        .map_err(|_| format!("invalid size number in `{text}`"))?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => return Err(format!("unknown size unit `{other}` in `{text}`")),
    };
    let bytes = (num * mult).round();
    if !(0.0..=1.8e19).contains(&bytes) {
        return Err(format!("size out of range: `{text}`"));
    }
    // The range check above keeps the value inside u64.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(bytes as u64)
}

fn is_glob(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

fn is_valid_id(id: &str) -> bool {
    let mut parts = id.split('.');
    let ok_part = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    };
    let first = parts.next().is_some_and(ok_part);
    let mut rest = 0;
    for p in parts {
        if !ok_part(p) {
            return false;
        }
        rest += 1;
    }
    first && rest > 0
}

fn is_token_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Parses a path pattern. `allow_glob` permits wildcard components and a
/// leading `**`.
///
/// # Errors
///
/// Returns a message describing the problem.
pub fn parse_path_pattern(text: &str, allow_glob: bool) -> Result<PathPattern, String> {
    let norm = text.replace('/', "\\");
    let (root, rest): (PatternRoot, &str) = if let Some(after) = norm.strip_prefix('{') {
        let close = after
            .find('}')
            .ok_or_else(|| format!("unterminated token in `{text}`"))?;
        let name = &after[..close];
        if !is_token_name(name) {
            return Err(format!("invalid token `{{{name}}}` in `{text}`"));
        }
        let rest = &after[close + 1..];
        if !(rest.is_empty() || rest.starts_with('\\')) {
            return Err(format!("token must be followed by `\\` in `{text}`"));
        }
        (PatternRoot::Token(name.to_string()), rest)
    } else if norm.starts_with("**") {
        if !allow_glob {
            return Err(format!("`**` is only allowed in path_glob: `{text}`"));
        }
        (PatternRoot::Anywhere, &norm[..])
    } else if let Some(unc) = norm.strip_prefix(r"\\") {
        let mut it = unc.splitn(3, '\\');
        let server = it.next().unwrap_or_default();
        let share = it.next().unwrap_or_default();
        if server.is_empty() || share.is_empty() || is_glob(server) || is_glob(share) {
            return Err(format!("invalid UNC root in `{text}`"));
        }
        let rest_off = 2 + server.len() + 1 + share.len();
        let root = format!(r"\\{}\{}", fold_name(server), fold_name(share));
        (
            PatternRoot::Absolute(root),
            &norm[rest_off.min(norm.len())..],
        )
    } else {
        let b = norm.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            (PatternRoot::Absolute(fold_name(&norm[..2])), &norm[2..])
        } else {
            return Err(format!(
                "path must start with a {{TOKEN}}, a drive, a UNC root or `**`: `{text}`"
            ));
        }
    };
    let mut components = Vec::new();
    for part in rest.split('\\') {
        match part {
            "" => {}
            "." | ".." => return Err(format!("`.`/`..` not allowed in `{text}`")),
            "**" => {
                if !allow_glob {
                    return Err(format!("`**` is only allowed in path_glob: `{text}`"));
                }
                if components.last() != Some(&Component::Deep) {
                    components.push(Component::Deep);
                }
            }
            p if is_glob(p) => {
                if !allow_glob {
                    return Err(format!("wildcards are only allowed in path_glob: `{text}`"));
                }
                if p.contains("**") {
                    return Err(format!("`**` must be a whole component in `{text}`"));
                }
                let folded = fold_name(p);
                globset::GlobBuilder::new(&folded)
                    .literal_separator(true)
                    .build()
                    .map_err(|e| format!("invalid glob component `{p}`: {e}"))?;
                components.push(Component::Glob(folded));
            }
            p => components.push(Component::Lit(fold_name(p))),
        }
    }
    if matches!(root, PatternRoot::Anywhere) {
        // The leading `**` became the root; drop the duplicate component.
        if components.first() == Some(&Component::Deep) {
            components.remove(0);
        }
        if components.is_empty() {
            return Err(format!("`**` alone matches everything: `{text}`"));
        }
    }
    if components.last() == Some(&Component::Deep) {
        return Err(format!(
            "trailing `**` is redundant (descendants inherit): `{text}`"
        ));
    }
    Ok(PathPattern {
        text: text.to_string(),
        root,
        components,
    })
}

fn parse_name_req(s: &str, allow_placeholders: bool) -> Result<NameReq, String> {
    match s {
        "{stem}" | "{original}" if !allow_placeholders => Err(format!(
            "placeholder `{s}` is only valid in requires_sibling"
        )),
        "{stem}" => Ok(NameReq::Stem),
        "{original}" => Ok(NameReq::Original),
        _ if s.contains(['\\', '/']) => Err(format!("name `{s}` must not contain separators")),
        _ if is_glob(s) => {
            let folded = fold_name(s);
            globset::Glob::new(&folded).map_err(|e| format!("invalid glob `{s}`: {e}"))?;
            Ok(NameReq::Glob(folded))
        }
        _ => Ok(NameReq::Lit(fold_name(s))),
    }
}

/// Expands `@list` references.
fn expand(
    values: Option<OneOrMany>,
    lists: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for v in values.map(OneOrMany::into_vec).unwrap_or_default() {
        if let Some(name) = v.strip_prefix('@') {
            let list = lists
                .get(name)
                .ok_or_else(|| format!("unknown list `@{name}`"))?;
            out.extend(list.iter().cloned());
        } else {
            out.push(v);
        }
    }
    Ok(out)
}

fn parse_category(s: &str) -> Result<Category, String> {
    let quoted = format!("\"{s}\"");
    serde_json::from_str::<Category>(&quoted).map_err(|_| {
        let names: Vec<String> = Category::ALL
            .iter()
            .filter_map(|c| serde_json::to_string(c).ok())
            .collect();
        format!(
            "unknown category `{s}` (expected one of {})",
            names.join(", ")
        )
    })
}

fn size_of(raw: Option<RawSize>) -> Result<Option<u64>, String> {
    match raw {
        None => Ok(None),
        Some(RawSize::Bytes(b)) => Ok(Some(b)),
        Some(RawSize::Text(t)) => parse_size(&t).map(Some),
    }
}

/// Validates a raw rule.
///
/// # Errors
///
/// Returns a message describing the first problem found.
pub fn validate_rule(
    raw: RawRule,
    pack: &str,
    source: &RuleSource,
    lists: &BTreeMap<String, Vec<String>>,
) -> Result<Rule, String> {
    if !is_valid_id(&raw.id) {
        return Err("id must be dotted lowercase segments (`pack.name`)".into());
    }
    if *source == RuleSource::Builtin && !raw.id.starts_with(&format!("{pack}.")) {
        return Err(format!("built-in rule ids must start with `{pack}.`"));
    }
    if raw.name.trim().is_empty() || raw.explain.trim().is_empty() {
        return Err("`name` and `explain` must not be empty".into());
    }
    let category = parse_category(&raw.category)?;
    let m = raw.matchers;

    let path = expand(m.path, lists)?;
    let path_glob = expand(m.path_glob, lists)?;
    let path_regex = expand(m.path_regex, lists)?;
    let path_kinds = usize::from(!path.is_empty())
        + usize::from(!path_glob.is_empty())
        + usize::from(!path_regex.is_empty());
    if path_kinds > 1 {
        return Err("use at most one of `path`, `path_glob`, `path_regex`".into());
    }

    let mut matchers = Matchers {
        path_is_glob: !path_glob.is_empty(),
        ..Matchers::default()
    };
    for p in &path {
        matchers.paths.push(parse_path_pattern(p, false)?);
    }
    for p in &path_glob {
        matchers.paths.push(parse_path_pattern(p, true)?);
    }
    for r in &path_regex {
        regex::RegexBuilder::new(r)
            .case_insensitive(true)
            .build()
            .map_err(|e| format!("invalid path_regex: {e}"))?;
        matchers.path_regex.push(r.clone());
    }

    for (values, kind) in [
        (expand(m.name, lists)?, NameKind::Any),
        (expand(m.dir_name, lists)?, NameKind::Dir),
        (expand(m.file_name, lists)?, NameKind::File),
    ] {
        for v in values {
            if v.is_empty() || v.contains(['\\', '/']) {
                return Err(format!("invalid name `{v}`"));
            }
            let glob = is_glob(&v);
            let folded = fold_name(&v);
            if glob {
                globset::Glob::new(&folded).map_err(|e| format!("invalid name glob `{v}`: {e}"))?;
            }
            matchers.names.push(NamePattern {
                pattern: folded,
                is_glob: glob,
                kind,
            });
        }
    }
    for r in expand(m.name_regex, lists)? {
        regex::RegexBuilder::new(&r)
            .case_insensitive(true)
            .build()
            .map_err(|e| format!("invalid name_regex: {e}"))?;
        matchers.name_regex.push(r);
    }
    for e in expand(m.ext, lists)? {
        let e = e.trim_start_matches('.');
        if e.is_empty() || e.contains(['\\', '/', '*', '?', '.']) {
            return Err(format!("invalid extension `{e}`"));
        }
        let mut f = String::new();
        fold_str_into(e, &mut f);
        matchers.exts.push(f);
    }
    for f in expand(m.flag, lists)? {
        matchers
            .flags
            .push(FlagMatch::parse(&f).ok_or_else(|| format!("unknown flag `{f}`"))?);
    }
    matchers.any = m.any;
    for s in expand(m.requires_sibling, lists)? {
        matchers.requires_sibling.push(parse_name_req(&s, true)?);
    }
    for s in expand(m.requires_child, lists)? {
        matchers.requires_child.push(parse_name_req(&s, false)?);
    }
    matchers.min_size = size_of(m.min_size)?;
    matchers.max_size = size_of(m.max_size)?;
    matchers.older_than_days = m.older_than_days;
    for t in expand(m.magic, lists)? {
        matchers
            .magic
            .push(DetectedType::from_name(&t).ok_or_else(|| format!("unknown magic type `{t}`"))?);
    }
    let has_dir_names = matchers.names.iter().any(|n| n.kind == NameKind::Dir);
    let has_file_names = matchers.names.iter().any(|n| n.kind == NameKind::File);
    matchers.applies_to = match (m.applies_to, has_dir_names, has_file_names) {
        (Some(a), _, _) => a,
        (None, true, false) => AppliesTo::Dir,
        (None, false, true) => AppliesTo::File,
        _ => AppliesTo::Both,
    };
    for u in expand(m.under, lists)? {
        matchers.under.push(parse_path_pattern(&u, true)?);
    }
    for u in expand(m.not_under, lists)? {
        matchers.not_under.push(parse_path_pattern(&u, true)?);
    }
    matchers.volume_root = m.volume_root;
    matchers.empty_dir = m.empty_dir;

    if matchers.primary() == MatchedBy::Any && !matchers.any {
        return Err("rule has no matcher (set `match.any = true` to match everything)".into());
    }
    if matchers.any && matchers.primary() != MatchedBy::Any {
        return Err(
            "`match.any` cannot be combined with a path, name, ext, flag or magic matcher".into(),
        );
    }
    if !matchers.path_regex.is_empty() && !matchers.under.iter().any(PathPattern::is_literal) {
        return Err(
            "`path_regex` requires a literal `under` root (it is evaluated only inside it)".into(),
        );
    }
    if (matchers.empty_dir || !matchers.requires_child.is_empty())
        && matchers.applies_to == AppliesTo::File
    {
        return Err("`empty_dir`/`requires_child` only apply to directories".into());
    }
    if matchers.any
        && matchers.min_size.is_none()
        && matchers.max_size.is_none()
        && !matchers.empty_dir
        && matchers.older_than_days.is_none()
        && matchers.under.is_empty()
    {
        return Err(
            "`match.any` needs at least one size, age, empty_dir or under constraint".into(),
        );
    }

    let action = raw.action.unwrap_or(if raw.safety == Safety::Never {
        Action::InfoOnly
    } else {
        Action::Delete
    });
    if raw.safety == Safety::Never && action == Action::Delete {
        return Err("`never` rules cannot offer `delete`".into());
    }
    if action == Action::OpenTool && raw.tool.is_none() {
        return Err("`action = \"open_tool\"` requires `tool`".into());
    }
    if action == Action::Delete && raw.tool.is_some() {
        return Err("`tool` requires `action = \"open_tool\"` or `\"info_only\"`".into());
    }
    let inherit = raw.inherit.unwrap_or_default();
    if raw.safety == Safety::Never && inherit != Inherit::Strong {
        return Err("`never` rules must use strong inheritance".into());
    }

    Ok(Rule {
        id: raw.id,
        name: raw.name,
        pack: pack.to_string(),
        source: source.clone(),
        category,
        subcategory: raw.subcategory,
        safety: raw.safety,
        regenerable: raw.regenerable,
        explain: raw.explain.trim().to_string(),
        action,
        tool: raw.tool,
        app: raw.app,
        inherit,
        matchers,
    })
}

impl fmt::Display for MatchedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Default => "default",
            Self::Any => "any",
            Self::Magic => "magic",
            Self::Ext => "ext",
            Self::Name => "name",
            Self::Flag => "flag",
            Self::PathRegex => "path_regex",
            Self::PathGlob => "path_glob",
            Self::Path => "path",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(toml_body: &str) -> Result<Rule, String> {
        let text = format!(
            "schema_version = 1\npack = \"t\"\n[[rule]]\nid = \"t.x\"\nname = \"X\"\nexplain = \"e\"\n{toml_body}"
        );
        let pack = parse_pack("t.toml", &text).map_err(|e| e.to_string())?;
        let raw = pack.rules.into_iter().next().ok_or("no rule")?;
        validate_rule(raw, "t", &RuleSource::Builtin, &pack.lists)
    }

    #[test]
    fn parses_spec_example() {
        let r = rule(
            r#"category = "dev_build"
subcategory = "dev.dependencies"
match.dir_name = "node_modules"
match.requires_sibling = ["package.json"]
app = "Node.js / npm"
safety = "safe"
regenerable = true
action = "delete""#,
        )
        .unwrap();
        assert_eq!(r.matchers.primary(), MatchedBy::Name);
        assert_eq!(r.matchers.applies_to, AppliesTo::Dir);
        assert_eq!(
            r.matchers.requires_sibling,
            vec![NameReq::Lit("PACKAGE.JSON".into())]
        );
    }

    #[test]
    fn rejects_bad_rules() {
        let base = "category = \"caches\"\nsafety = \"safe\"\n";
        assert!(
            rule(&format!("{base}match.dir_nam = \"x\"")).is_err(),
            "typo"
        );
        assert!(rule(base).is_err(), "no matchers");
        assert!(rule("category = \"nope\"\nsafety = \"safe\"\nmatch.ext = \"x\"").is_err());
        assert!(
            rule(
                "category = \"system\"\nsafety = \"never\"\naction = \"delete\"\nmatch.ext = \"x\""
            )
            .is_err()
        );
        assert!(
            rule(
                "category = \"system\"\nsafety = \"never\"\ninherit = \"weak\"\nmatch.ext = \"x\""
            )
            .is_err()
        );
        assert!(rule(&format!("{base}action = \"open_tool\"\nmatch.ext = \"x\"")).is_err());
        assert!(
            rule(&format!("{base}match.path = \"{{X}}\\\\*\"")).is_err(),
            "glob in path"
        );
        assert!(rule(&format!("{base}match.path = \"relative\\\\x\"")).is_err());
        assert!(
            rule(&format!("{base}match.path_regex = \".*\"")).is_err(),
            "regex w/o under"
        );
        assert!(
            rule(&format!("{base}match.any = true")).is_err(),
            "unbounded any"
        );
        assert!(rule(&format!("{base}match.path_glob = \"{{X}}\\\\a\\\\**\"")).is_err());
        assert!(
            rule(&format!(
                "{base}match.path = \"{{X}}\"\nmatch.path_glob = \"{{X}}\""
            ))
            .is_err()
        );
    }

    #[test]
    fn parses_patterns() {
        let p =
            parse_path_pattern(r"{LOCALAPPDATA}\Google\Chrome*\User Data\*\Cache", true).unwrap();
        assert_eq!(p.root, PatternRoot::Token("LOCALAPPDATA".into()));
        assert_eq!(
            p.components,
            vec![
                Component::Lit("GOOGLE".into()),
                Component::Glob("CHROME*".into()),
                Component::Lit("USER DATA".into()),
                Component::Glob("*".into()),
                Component::Lit("CACHE".into()),
            ]
        );
        let p = parse_path_pattern(r"**\steamapps\shadercache", true).unwrap();
        assert_eq!(p.root, PatternRoot::Anywhere);
        assert_eq!(p.components.len(), 2);
        let p = parse_path_pattern(r"\\nas\share\x", false).unwrap();
        assert_eq!(p.root, PatternRoot::Absolute(r"\\NAS\SHARE".into()));
        let p = parse_path_pattern("d:/data", false).unwrap();
        assert_eq!(p.root, PatternRoot::Absolute("D:".into()));
        assert!(parse_path_pattern(r"{bad}\x", false).is_err());
        assert!(parse_path_pattern(r"{X}y", false).is_err());
    }

    #[test]
    fn expands_lists() {
        let text = r#"schema_version = 1
pack = "t"
[lists]
caches = ["Cache", "GPUCache"]
[[rule]]
id = "t.x"
name = "X"
explain = "e"
category = "caches"
safety = "safe"
match.dir_name = ["@caches", "Other"]
"#;
        let pack = parse_pack("t.toml", text).unwrap();
        let raw = pack.rules[0].clone();
        let r = validate_rule(raw, "t", &RuleSource::Builtin, &pack.lists).unwrap();
        assert_eq!(r.matchers.names.len(), 3);
        let mut bad = pack.rules[0].clone();
        bad.matchers.dir_name = Some(OneOrMany::One("@missing".into()));
        assert!(validate_rule(bad, "t", &RuleSource::Builtin, &pack.lists).is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1 KiB"), Ok(1024));
        assert_eq!(parse_size("2kb"), Ok(2000));
        assert!(parse_size("1 parsec").is_err());
        assert!(parse_size("abc").is_err());
    }
}
