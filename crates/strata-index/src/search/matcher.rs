//! Compiled name matchers.
//!
//! Case-insensitive matching folds the name once per candidate (ASCII fast
//! path, Unicode fallback; see [`crate::fold`]) and runs literal searches with
//! `memchr::memmem`. Wildcards are prefiltered by their longest literal run.

use memchr::memmem;
use regex::bytes::Regex;

use super::parse::{QueryError, Term};
use crate::fold;

/// Relevance points by match quality.
pub(crate) const SCORE_EXACT: u32 = 1000;
pub(crate) const SCORE_PREFIX: u32 = 600;
pub(crate) const SCORE_WORD: u32 = 400;
pub(crate) const SCORE_SUBSTRING: u32 = 200;
pub(crate) const SCORE_PATTERN: u32 = 300;

#[derive(Debug, Clone)]
pub(crate) enum Matcher {
    Substring(Box<memmem::Finder<'static>>),
    Prefix(Vec<u8>),
    Glob(Box<Glob>),
    Regex(Regex),
    /// Outermost component first; the last one matches the entry name.
    Path(Vec<Component>),
}

#[derive(Debug, Clone)]
pub(crate) enum Component {
    Exact(Vec<u8>),
    Glob(Box<Glob>),
}

impl Component {
    fn matches(&self, folded: &[u8]) -> bool {
        match self {
            Self::Exact(e) => e == folded,
            Self::Glob(g) => g.matches(folded),
        }
    }
}

impl Matcher {
    pub(crate) fn compile(term: &Term, case_sensitive: bool) -> Result<Self, QueryError> {
        let prep = |s: &str| -> Vec<u8> {
            if case_sensitive {
                s.as_bytes().to_vec()
            } else {
                fold::fold(s.as_bytes())
            }
        };
        Ok(match term {
            Term::Substring(s) => {
                Self::Substring(Box::new(memmem::Finder::new(&prep(s)).into_owned()))
            }
            Term::Prefix(s) => Self::Prefix(prep(s)),
            Term::Wildcard(s) => Self::Glob(Box::new(Glob::new(&prep(s)))),
            Term::Regex(r) => {
                let pat = if case_sensitive {
                    r.clone()
                } else {
                    format!("(?i){r}")
                };
                Self::Regex(Regex::new(&pat).map_err(|e| QueryError::BadRegex(e.to_string()))?)
            }
            Term::Path(parts) => Self::Path(
                parts
                    .iter()
                    .map(|p| {
                        let b = prep(p);
                        if p.contains(['*', '?']) {
                            Component::Glob(Box::new(Glob::new(&b)))
                        } else {
                            Component::Exact(b)
                        }
                    })
                    .collect(),
            ),
        })
    }

    /// Whether the matcher needs the raw (unfolded) name.
    pub(crate) fn wants_raw(&self) -> bool {
        matches!(self, Self::Regex(_))
    }

    /// Matches the entry name. `name` is folded unless the query is
    /// case-sensitive; `raw` is the stored name. Path matchers only check the
    /// last component here (ancestors are checked by the engine). Returns the
    /// relevance score, or `None`.
    #[inline]
    pub(crate) fn score(&self, name: &[u8], raw: &[u8]) -> Option<u32> {
        match self {
            Self::Substring(f) => {
                let needle = f.needle();
                if needle.is_empty() {
                    return Some(0);
                }
                let pos = f.find(name)?;
                Some(if pos == 0 && needle.len() == name.len() {
                    SCORE_EXACT
                } else if pos == 0 {
                    SCORE_PREFIX
                } else if !name[pos - 1].is_ascii_alphanumeric() {
                    SCORE_WORD
                } else {
                    SCORE_SUBSTRING
                })
            }
            Self::Prefix(p) => name.starts_with(p).then_some(if p.len() == name.len() {
                SCORE_EXACT
            } else {
                SCORE_PREFIX
            }),
            Self::Glob(g) => g.matches(name).then_some(SCORE_PATTERN),
            Self::Regex(r) => r.is_match(raw).then_some(SCORE_PATTERN),
            Self::Path(parts) => parts
                .last()
                .is_some_and(|c| c.matches(name))
                .then_some(SCORE_PATTERN),
        }
    }

    pub(crate) fn path_components(&self) -> Option<&[Component]> {
        match self {
            Self::Path(p) => Some(p),
            _ => None,
        }
    }

    pub(crate) fn component_matches(c: &Component, folded: &[u8]) -> bool {
        c.matches(folded)
    }
}

// -----------------------------------------------------------------------------
// Wildcards
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Byte(u8),
    /// `?`: exactly one character (one UTF-8/WTF-8 sequence).
    One,
    /// `*`: any run.
    Star,
}

/// A `*`/`?` pattern matched against a whole name.
#[derive(Debug, Clone)]
pub(crate) struct Glob {
    toks: Vec<Tok>,
    /// Longest literal run, used as a `memmem` prefilter.
    literal: Option<memmem::Finder<'static>>,
}

impl Glob {
    pub(crate) fn new(pat: &[u8]) -> Self {
        let mut toks = Vec::with_capacity(pat.len());
        for &b in pat {
            let t = match b {
                b'*' => Tok::Star,
                b'?' => Tok::One,
                _ => Tok::Byte(b),
            };
            if t == Tok::Star && toks.last() == Some(&Tok::Star) {
                continue;
            }
            toks.push(t);
        }
        let mut best: &[u8] = &[];
        for run in pat.split(|&b| b == b'*' || b == b'?') {
            if run.len() > best.len() {
                best = run;
            }
        }
        let literal = (best.len() >= 2).then(|| memmem::Finder::new(best).into_owned());
        Self { toks, literal }
    }

    pub(crate) fn matches(&self, s: &[u8]) -> bool {
        if let Some(f) = &self.literal
            && f.find(s).is_none()
        {
            return false;
        }
        let p = &self.toks;
        let (mut pi, mut si) = (0usize, 0usize);
        let mut star: Option<(usize, usize)> = None;
        while si < s.len() {
            match p.get(pi) {
                Some(Tok::Byte(b)) if *b == s[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                Some(Tok::One) => {
                    pi += 1;
                    si += char_len(s, si);
                    continue;
                }
                Some(Tok::Star) => {
                    star = Some((pi, si));
                    pi += 1;
                    continue;
                }
                _ => {}
            }
            match star {
                Some((sp, ss)) => {
                    let next = ss + char_len(s, ss);
                    star = Some((sp, next));
                    pi = sp + 1;
                    si = next;
                }
                None => return false,
            }
        }
        while p.get(pi) == Some(&Tok::Star) {
            pi += 1;
        }
        pi == p.len() && si == s.len()
    }
}

/// Byte length of the UTF-8/WTF-8 sequence starting at `s[i]` (1 for stray
/// bytes), so `?` and `*` backtracking step over whole characters.
#[inline]
fn char_len(s: &[u8], i: usize) -> usize {
    let n = match s[i] {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    };
    n.min(s.len() - i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(p: &str, s: &str) -> bool {
        Glob::new(p.as_bytes()).matches(s.as_bytes())
    }

    #[test]
    fn globs() {
        assert!(g("*.gguf", "model.gguf"));
        assert!(!g("*.gguf", "model.gguf.part"));
        assert!(g("img_??.png", "img_01.png"));
        assert!(!g("img_??.png", "img_1.png"));
        assert!(g("a*b*c", "aXXbYYc"));
        assert!(!g("a*b*c", "aXXbYY"));
        assert!(g("*", ""));
        assert!(g("?", "é"));
        assert!(g("caf?", "café"));
        assert!(!g("abc", "abcd"));
    }

    #[test]
    fn substring_scores() {
        let m = Matcher::compile(&Term::Substring("Report".into()), false).unwrap();
        let s = |n: &str| m.score(&fold::fold(n.as_bytes()), n.as_bytes());
        assert_eq!(s("report"), Some(SCORE_EXACT));
        assert_eq!(s("Report.pdf"), Some(SCORE_PREFIX));
        assert_eq!(s("q3-report.pdf"), Some(SCORE_WORD));
        assert_eq!(s("myreport"), Some(SCORE_SUBSTRING));
        assert_eq!(s("repo"), None);
    }

    #[test]
    fn regex_is_case_insensitive_by_default() {
        let m = Matcher::compile(&Term::Regex(r"^IMG_\d+".into()), false).unwrap();
        assert!(m.score(b"", b"img_123.jpg").is_some());
        let m = Matcher::compile(&Term::Regex(r"^IMG_\d+".into()), true).unwrap();
        assert!(m.score(b"", b"img_123.jpg").is_none());
    }
}
