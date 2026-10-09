//! Query language (SPEC §17).
//!
//! A query is whitespace-separated tokens; all of them must hold (AND).
//! Double quotes group a phrase (`"my file"`). Matching is case-insensitive
//! unless `case:yes` is given.
//!
//! **Name terms** (matched against the entry name):
//!
//! | syntax | meaning |
//! |---|---|
//! | `foo` | substring (default) |
//! | `^foo` | prefix |
//! | `*.gguf`, `img_??.png` | wildcard over the whole name (`*` any run, `?` one character) |
//! | `re:^\d+$`, `/^\d+$/` | regular expression (the UI regex toggle uses [`Query::parse_regex`]) |
//! | `node_modules\react` | path components: the entry is named `react` and its parent `node_modules` (each component is an exact name or a wildcard; `/` also separates) |
//!
//! **Filters** (`key:value`; repeated filters AND together, comma-separated
//! values within one filter are alternatives):
//!
//! | filter | values |
//! |---|---|
//! | `size:` | `>1gb`, `>=1gb`, `<10mb`, `<=`, `5mb` (exact), `1mb..5mb`; units b, kb, mb, gb, tb (binary), decimals allowed |
//! | `modified:` | `<30d` (within the last 30 days), `>1y` (older than a year), units h, d, w, m (30 d), y (365 d); `>2024-01-01`, `<2024-01-01`, `2024-01-01` (that day), `2024-01-01..2024-06-30`, `today` |
//! | `ext:` | `mp4`, `.mp4`, `mp4,mkv` |
//! | `app:` | owning app name contains the value (resolved by the app provider) |
//! | `cat:` | category whose name starts with the value (`cache`, `ai`, `dev`, `media`, ...) |
//! | `safe:` | `yes` (safe tier), `no` (any other tier), or a tier: `safe`, `probably`, `careful`, `never` |
//! | `dir:` / `file:` | only directories / files; a value adds a substring term |
//! | `vol:` | volume letter, e.g. `vol:D` |
//! | `attr:` | flags that must all be set: `hidden`, `system`, `readonly`, `compressed`, `sparse`, `encrypted`, `ads`, `hardlink`, `orphan`, `reparse`, `symlink`, `junction`, `temporary`, `offline`, `partial`, `denied`, `suspicious` |
//! | `cloud:` | `online`, `local`, `pinned` |
//! | `case:` | `yes` for case-sensitive name matching |
//!
//! A token whose key is not a known filter is treated as a name term.

use strata_core::{Category, CloudState, EntryFlags, EpochSecs, FileTime, ReparseKind, Safety};

use crate::query::{EntryKind, Filter};

/// A parsed search query.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    /// Name terms; all must match.
    pub terms: Vec<Term>,
    /// Column filters.
    pub filter: Filter,
    /// Safety filter (needs a safety provider).
    pub safety: Option<SafetyFilter>,
    /// `app:` values (any of); needs an app provider.
    pub apps: Vec<String>,
    /// `vol:` letter, uppercase.
    pub volume: Option<char>,
    /// Case-sensitive name matching.
    pub case_sensitive: bool,
    /// `ext:` values, lowercased without the dot (any of). Resolved to
    /// extension ids per index at search time.
    pub ext_names: Vec<String>,
    /// `attr:reparse|symlink|junction`: reparse kinds (any of).
    pub reparse_kinds: Vec<ReparseKind>,
}

/// One name term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    /// Substring of the name.
    Substring(String),
    /// Prefix of the name.
    Prefix(String),
    /// Whole-name wildcard pattern (`*`, `?`).
    Wildcard(String),
    /// Regular expression over the name.
    Regex(String),
    /// Trailing path components, outermost first.
    Path(Vec<String>),
}

/// `safe:` filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyFilter {
    /// Only the given tier.
    Tier(Safety),
    /// Anything but [`Safety::Safe`].
    NotSafe,
}

/// Why a query failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    /// A filter value could not be understood.
    #[error("invalid value {value:?} for {key}:")]
    BadValue {
        /// Filter key.
        key: String,
        /// Offending value.
        value: String,
    },
    /// A regular expression failed to compile.
    #[error("invalid regular expression: {0}")]
    BadRegex(String),
    /// An unterminated quote.
    #[error("unterminated quote")]
    UnterminatedQuote,
}

const SECS_PER_DAY: i64 = 86_400;

impl Query {
    /// Parses `text`. Relative dates (`modified:<30d`) are resolved against
    /// `now`.
    ///
    /// # Errors
    ///
    /// [`QueryError`] for malformed filter values, regexes or quotes.
    ///
    /// # Example
    ///
    /// ```
    /// use strata_core::FileTime;
    /// use strata_index::search::{Query, Term};
    /// let q = Query::parse(r#"*.gguf size:>1gb node_modules\react "my file""#, FileTime(0)).unwrap();
    /// assert_eq!(q.terms[0], Term::Wildcard("*.gguf".into()));
    /// assert_eq!(q.filter.size, Some(((1 << 30) + 1, u64::MAX)));
    /// assert_eq!(q.terms[1], Term::Path(vec!["node_modules".into(), "react".into()]));
    /// assert_eq!(q.terms[2], Term::Substring("my file".into()));
    /// ```
    pub fn parse(text: &str, now: FileTime) -> Result<Self, QueryError> {
        Self::parse_inner(text, now, false)
    }

    /// Parses `text` with the regex toggle on: every name term (quoted or
    /// not) is a regular expression; filters work as in [`Query::parse`].
    ///
    /// # Errors
    ///
    /// [`QueryError`] for malformed filters, regexes or quotes.
    pub fn parse_regex(text: &str, now: FileTime) -> Result<Self, QueryError> {
        Self::parse_inner(text, now, true)
    }

    fn parse_inner(text: &str, now: FileTime, regex: bool) -> Result<Self, QueryError> {
        let mut q = Self::default();
        for (tok, quoted) in tokenize(text)? {
            if tok.is_empty() {
                continue;
            }
            if !quoted
                && let Some((key, value)) = tok.split_once(':')
                && q.apply_filter(&key.to_ascii_lowercase(), value, now)?
            {
                continue;
            }
            q.terms.push(if regex {
                validate_regex(&tok)?;
                Term::Regex(tok)
            } else if quoted {
                Term::Substring(tok)
            } else {
                classify(&tok)?
            });
        }
        Ok(q)
    }

    /// Applies `key:value`; returns `false` if `key` is not a filter.
    fn apply_filter(&mut self, key: &str, value: &str, now: FileTime) -> Result<bool, QueryError> {
        let bad = || QueryError::BadValue {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        let lower = value.to_lowercase();
        match key {
            "size" => {
                let (lo, hi) = parse_range(&lower, parse_size).ok_or_else(bad)?;
                self.filter.size = Some(intersect(self.filter.size, (lo, hi)));
            }
            "modified" | "mtime" | "date" => {
                let (lo, hi) = parse_time_range(&lower, now).ok_or_else(bad)?;
                let r = intersect(
                    self.filter
                        .modified
                        .map(|(a, b)| (u64::from(a.0), u64::from(b.0))),
                    (lo, hi),
                );
                let c = |v: u64| EpochSecs(u32::try_from(v).unwrap_or(u32::MAX));
                self.filter.modified = Some((c(r.0), c(r.1)));
            }
            "ext" => {
                for e in split_list(&lower) {
                    let e = e.trim_start_matches('.');
                    if e.is_empty() {
                        return Err(bad());
                    }
                    self.ext_names.push(e.to_owned());
                }
            }
            "app" => {
                if lower.is_empty() {
                    return Err(bad());
                }
                self.apps.extend(split_list(&lower).map(str::to_owned));
            }
            "cat" | "category" => {
                let mut any = false;
                for v in split_list(&lower) {
                    for c in Category::ALL {
                        if category_matches(c, v) && !self.filter.categories.contains(&(c as u16)) {
                            self.filter.categories.push(c as u16);
                            any = true;
                        }
                    }
                }
                if !any {
                    return Err(bad());
                }
            }
            "safe" | "safety" => {
                self.safety = Some(match lower.as_str() {
                    "yes" | "safe" => SafetyFilter::Tier(Safety::Safe),
                    "no" => SafetyFilter::NotSafe,
                    "probably" => SafetyFilter::Tier(Safety::Probably),
                    "careful" => SafetyFilter::Tier(Safety::Careful),
                    "never" => SafetyFilter::Tier(Safety::Never),
                    _ => return Err(bad()),
                });
            }
            "dir" | "folder" | "file" => {
                self.filter.kind = if key == "file" {
                    EntryKind::Files
                } else {
                    EntryKind::Dirs
                };
                if !value.is_empty() {
                    self.terms.push(classify(value)?);
                }
            }
            "vol" | "volume" => {
                let c = value
                    .trim_end_matches([':', '\\'])
                    .chars()
                    .next()
                    .filter(char::is_ascii_alphabetic)
                    .ok_or_else(bad)?;
                self.volume = Some(c.to_ascii_uppercase());
            }
            "attr" | "attrib" => {
                for a in split_list(&lower) {
                    self.add_attr(a).ok_or_else(bad)?;
                }
            }
            "cloud" => {
                for v in split_list(&lower) {
                    self.filter.cloud.push(match v {
                        "online" | "online-only" => CloudState::OnlineOnly,
                        "local" | "available" => CloudState::LocallyAvailable,
                        "pinned" | "keep" => CloudState::AlwaysKeep,
                        _ => return Err(bad()),
                    });
                }
            }
            "case" => {
                self.case_sensitive = match lower.as_str() {
                    "yes" | "on" | "true" => true,
                    "no" | "off" | "false" => false,
                    _ => return Err(bad()),
                };
            }
            "re" | "regex" => {
                validate_regex(value)?;
                self.terms.push(Term::Regex(value.to_owned()));
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn add_attr(&mut self, a: &str) -> Option<()> {
        let flag = match a {
            "hidden" => EntryFlags::HIDDEN,
            "system" => EntryFlags::SYSTEM,
            "readonly" => EntryFlags::READONLY,
            "compressed" => EntryFlags::COMPRESSED,
            "sparse" => EntryFlags::SPARSE,
            "encrypted" => EntryFlags::ENCRYPTED,
            "ads" => EntryFlags::HAS_ADS,
            "hardlink" => EntryFlags::HARDLINK_SECONDARY,
            "orphan" => EntryFlags::ORPHAN,
            "temporary" | "temp" => EntryFlags::TEMPORARY,
            "offline" => EntryFlags::OFFLINE,
            "partial" => EntryFlags::PARTIAL,
            "denied" => EntryFlags::ACCESS_DENIED,
            "suspicious" => EntryFlags::SUSPICIOUS_TIME,
            "reparse" | "symlink" | "junction" => {
                let kinds: &[ReparseKind] = match a {
                    "symlink" => &[ReparseKind::Symlink, ReparseKind::Wsl],
                    "junction" => &[ReparseKind::MountPoint],
                    _ => &[
                        ReparseKind::Symlink,
                        ReparseKind::MountPoint,
                        ReparseKind::Wof,
                        ReparseKind::Cloud,
                        ReparseKind::Dedup,
                        ReparseKind::AppExecLink,
                        ReparseKind::Wsl,
                        ReparseKind::Unknown,
                    ],
                };
                self.reparse_kinds.extend_from_slice(kinds);
                return Some(());
            }
            _ => return None,
        };
        self.filter.flags_all |= flag;
        Some(())
    }
}

/// Splits a raw query into tokens; the flag is `true` for quoted phrases.
fn tokenize(text: &str) -> Result<Vec<(String, bool)>, QueryError> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' && cur.is_empty() {
            let mut phrase = String::new();
            let mut closed = false;
            for c in chars.by_ref() {
                if c == '"' {
                    closed = true;
                    break;
                }
                phrase.push(c);
            }
            if !closed {
                return Err(QueryError::UnterminatedQuote);
            }
            out.push((phrase, true));
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                out.push((std::mem::take(&mut cur), false));
            }
        } else if c == '"' {
            // `key:"value with spaces"`
            let mut closed = false;
            for c in chars.by_ref() {
                if c == '"' {
                    closed = true;
                    break;
                }
                cur.push(c);
            }
            if !closed {
                return Err(QueryError::UnterminatedQuote);
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push((cur, false));
    }
    Ok(out)
}

/// Classifies a bare word as a name term.
fn classify(tok: &str) -> Result<Term, QueryError> {
    if tok.len() >= 2 && tok.starts_with('/') && tok.ends_with('/') {
        let re = &tok[1..tok.len() - 1];
        validate_regex(re)?;
        return Ok(Term::Regex(re.to_owned()));
    }
    if let Some(rest) = tok.strip_prefix('^')
        && !rest.is_empty()
        && !rest.contains(['\\', '/', '*', '?'])
    {
        return Ok(Term::Prefix(rest.to_owned()));
    }
    if tok.contains(['\\', '/']) {
        let parts: Vec<String> = tok
            .split(['\\', '/'])
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        if parts.len() == 1 {
            return Ok(classify_simple(&parts[0]));
        }
        if !parts.is_empty() {
            return Ok(Term::Path(parts));
        }
    }
    Ok(classify_simple(tok))
}

fn classify_simple(tok: &str) -> Term {
    if tok.contains(['*', '?']) {
        Term::Wildcard(tok.to_owned())
    } else {
        Term::Substring(tok.to_owned())
    }
}

fn validate_regex(re: &str) -> Result<(), QueryError> {
    regex::bytes::Regex::new(re)
        .map(|_| ())
        .map_err(|e| QueryError::BadRegex(e.to_string()))
}

fn split_list(v: &str) -> impl Iterator<Item = &str> {
    v.split([',', ';', '|'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn category_matches(c: Category, v: &str) -> bool {
    let serde_name = match c {
        Category::Unknown => "unknown",
        Category::System => "system",
        Category::Apps => "apps",
        Category::Games => "games",
        Category::AiModels => "ai_models",
        Category::DevBuild => "dev_build",
        Category::Caches => "caches",
        Category::Temp => "temp",
        Category::Downloads => "downloads",
        Category::Documents => "documents",
        Category::Media => "media",
        Category::Archives => "archives",
        Category::Cloud => "cloud",
        Category::RecycleBin => "recycle_bin",
        Category::NtfsMetadata => "ntfs_metadata",
    };
    serde_name.starts_with(v) || c.label().to_lowercase().starts_with(v)
}

fn intersect(cur: Option<(u64, u64)>, new: (u64, u64)) -> (u64, u64) {
    match cur {
        Some((a, b)) => (a.max(new.0), b.min(new.1)),
        None => new,
    }
}

/// Parses `>x`, `>=x`, `<x`, `<=x`, `=x`, `x`, `x..y` into an inclusive range.
fn parse_range(v: &str, unit: impl Fn(&str) -> Option<u64>) -> Option<(u64, u64)> {
    if let Some((a, b)) = v.split_once("..") {
        let lo = if a.is_empty() { 0 } else { unit(a)? };
        let hi = if b.is_empty() { u64::MAX } else { unit(b)? };
        return (lo <= hi).then_some((lo, hi));
    }
    if let Some(x) = v.strip_prefix(">=") {
        return Some((unit(x)?, u64::MAX));
    }
    if let Some(x) = v.strip_prefix("<=") {
        return Some((0, unit(x)?));
    }
    if let Some(x) = v.strip_prefix('>') {
        return Some((unit(x)?.checked_add(1)?, u64::MAX));
    }
    if let Some(x) = v.strip_prefix('<') {
        return Some((0, unit(x)?.checked_sub(1)?));
    }
    let x = unit(v.strip_prefix('=').unwrap_or(v))?;
    Some((x, x))
}

/// `1.5gb` → bytes (binary units).
pub(crate) fn parse_size(v: &str) -> Option<u64> {
    let v = v.trim();
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(v.len());
    let (num, unit) = v.split_at(split);
    if num.is_empty() {
        return None;
    }
    let mult: u64 = match unit.trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return None,
    };
    if let Ok(n) = num.parse::<u64>() {
        return n.checked_mul(mult);
    }
    let f: f64 = num.parse().ok()?;
    let bytes = f * mult as f64;
    (bytes.is_finite() && bytes >= 0.0 && bytes < u64::MAX as f64).then_some(bytes.round() as u64)
}

/// Seconds since 2000-01-01 of `now`.
fn epoch_now(now: FileTime) -> i64 {
    i64::from(EpochSecs::from_filetime(now).0)
}

/// `30d` → seconds.
fn parse_age(v: &str) -> Option<i64> {
    let split = v.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = v.split_at(split);
    let n: i64 = num.parse().ok()?;
    let mult = match unit {
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
        "d" | "day" | "days" => SECS_PER_DAY,
        "w" | "wk" | "week" | "weeks" => 7 * SECS_PER_DAY,
        "m" | "mo" | "month" | "months" => 30 * SECS_PER_DAY,
        "y" | "yr" | "year" | "years" => 365 * SECS_PER_DAY,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// `YYYY-MM-DD` → seconds since 2000-01-01 (UTC midnight).
fn parse_date(v: &str) -> Option<i64> {
    let mut it = v.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm), relative to 2000-03-01.
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_since_0000_03_01 = era * 146_097 + doe;
    // 2000-01-01 is day 730_425 in this count.
    Some((days_since_0000_03_01 - 730_425) * SECS_PER_DAY)
}

/// Parses a `modified:` value into an inclusive range of epoch seconds.
fn parse_time_range(v: &str, now: FileTime) -> Option<(u64, u64)> {
    let now_s = epoch_now(now);
    let clamp = |s: i64| s.clamp(0, i64::from(u32::MAX)) as u64;
    if v == "today" {
        let start = now_s - now_s.rem_euclid(SECS_PER_DAY);
        return Some((clamp(start), u64::from(u32::MAX)));
    }
    if let Some((a, b)) = v.split_once("..") {
        let lo = if a.is_empty() { 0 } else { parse_date(a)? };
        let hi = if b.is_empty() {
            i64::from(u32::MAX)
        } else {
            parse_date(b)? + SECS_PER_DAY - 1
        };
        return (lo <= hi).then(|| (clamp(lo), clamp(hi)));
    }
    let (op, rest) = if let Some(r) = v.strip_prefix(">=") {
        (">", r)
    } else if let Some(r) = v.strip_prefix("<=") {
        ("<", r)
    } else if let Some(r) = v.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = v.strip_prefix('<') {
        ("<", r)
    } else {
        ("=", v.strip_prefix('=').unwrap_or(v))
    };
    if let Some(day) = parse_date(rest) {
        return Some(match op {
            ">" => (clamp(day), u64::from(u32::MAX)),
            "<" => (0, clamp(day - 1)),
            _ => (clamp(day), clamp(day + SECS_PER_DAY - 1)),
        });
    }
    let age = parse_age(rest)?;
    let cutoff = now_s - age;
    Some(match op {
        // "modified:<30d": younger than 30 days.
        "<" => (clamp(cutoff), u64::from(u32::MAX)),
        // "modified:>1y": older than a year.
        ">" => (0, clamp(cutoff)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> FileTime {
        FileTime::from_unix_secs(1_760_000_000)
    }

    #[test]
    fn terms() {
        let q = Query::parse(r"foo ^bar *.mp4 re:\d+ /x$/ a\b\c", now()).unwrap();
        assert_eq!(
            q.terms,
            vec![
                Term::Substring("foo".into()),
                Term::Prefix("bar".into()),
                Term::Wildcard("*.mp4".into()),
                Term::Regex(r"\d+".into()),
                Term::Regex("x$".into()),
                Term::Path(vec!["a".into(), "b".into(), "c".into()]),
            ]
        );
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1gb"), Some(1 << 30));
        assert_eq!(parse_size("1.5kb"), Some(1536));
        assert_eq!(parse_size("12"), Some(12));
        assert_eq!(parse_size("1xb"), None);
        let q = Query::parse("size:1mb..5mb", now()).unwrap();
        assert_eq!(q.filter.size, Some((1 << 20, 5 << 20)));
        let q = Query::parse("size:<1kb size:>=10", now()).unwrap();
        assert_eq!(q.filter.size, Some((10, 1023)));
        assert!(Query::parse("size:big", now()).is_err());
    }

    #[test]
    fn dates() {
        assert_eq!(parse_date("2000-01-01"), Some(0));
        assert_eq!(parse_date("2000-01-02"), Some(SECS_PER_DAY));
        assert_eq!(parse_date("2024-03-01"), Some(8_826 * SECS_PER_DAY));
        let q = Query::parse("modified:<30d", now()).unwrap();
        let (lo, hi) = q.filter.modified.unwrap();
        assert_eq!(i64::from(lo.0), epoch_now(now()) - 30 * SECS_PER_DAY);
        assert_eq!(hi.0, u32::MAX);
        let q = Query::parse("modified:>2024-01-01", now()).unwrap();
        assert_eq!(
            i64::from(q.filter.modified.unwrap().0.0),
            parse_date("2024-01-01").unwrap()
        );
        assert!(Query::parse("modified:soon", now()).is_err());
    }

    #[test]
    fn filters() {
        let q = Query::parse(
            "ext:MP4,.mkv cat:cache safe:no dir: vol:d: attr:hidden,junction cloud:online case:yes app:claude",
            now(),
        )
        .unwrap();
        assert_eq!(q.ext_names, vec!["mp4".to_owned(), "mkv".to_owned()]);
        assert_eq!(q.filter.categories, vec![Category::Caches as u16]);
        assert_eq!(q.safety, Some(SafetyFilter::NotSafe));
        assert_eq!(q.filter.kind, EntryKind::Dirs);
        assert_eq!(q.volume, Some('D'));
        assert!(q.filter.flags_all.contains(EntryFlags::HIDDEN));
        assert_eq!(q.reparse_kinds, vec![ReparseKind::MountPoint]);
        assert_eq!(q.filter.cloud, vec![CloudState::OnlineOnly]);
        assert!(q.case_sensitive);
        assert_eq!(q.apps, vec!["claude".to_owned()]);
        assert!(q.terms.is_empty());
    }

    #[test]
    fn errors_and_fallbacks() {
        assert_eq!(
            Query::parse("\"open", now()),
            Err(QueryError::UnterminatedQuote)
        );
        assert!(matches!(
            Query::parse("re:(", now()),
            Err(QueryError::BadRegex(_))
        ));
        let q = Query::parse("odd:thing", now()).unwrap();
        assert_eq!(q.terms, vec![Term::Substring("odd:thing".into())]);
    }
}
