//! "Why is this classified as X?": path explanations for the settings UI.
//!
//! [`Classifier::explain`] walks a path from its volume root, classifying
//! each ancestor with real sibling/child facts from an [`FsProbe`], and
//! records every candidate rule considered for the final entry, which
//! matcher failed for the losers, and why the winner won.
//!
//! The app normally supplies a probe backed by its in-memory index (which has
//! subtree sizes and newest-modified times). [`StdFsProbe`] reads the real
//! filesystem and is meant for tools and tests: it reports directory sizes
//! as 0 and a directory's own modified time instead of its subtree's.

use std::fmt::Write as _;

use serde::Serialize;
use strata_core::{EntryFlags, FileTime, Safety};

use crate::engine::{ChildRef, Classification, Classifier, Decision, Entry, Name, Trace};
use crate::fold::{join_components, path_components_raw};
use crate::schema::{Action, MatchedBy, RuleSource, Tool};
use crate::sniff::DetectedType;

/// Facts about one entry, from a probe.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProbeEntry {
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// Size (subtree total for directories when known).
    pub size: u64,
    /// Newest modification (subtree newest for directories when known).
    pub newest_mtime: Option<FileTime>,
    /// Entry flags (the `DIR` bit is derived from `is_dir`).
    pub flags: EntryFlags,
    /// Sniffed type, when known.
    pub magic: Option<DetectedType>,
}

/// Source of filesystem facts for explanations.
pub trait FsProbe {
    /// Children of a directory as `(name, is_dir)`; `None` if unreadable.
    fn children(&self, dir: &str) -> Option<Vec<(String, bool)>>;
    /// Facts about an entry; `None` if it does not exist.
    fn entry(&self, path: &str) -> Option<ProbeEntry>;
}

/// An [`FsProbe`] over the real filesystem (read-only, never follows
/// reparse points, never opens file contents).
#[derive(Debug, Clone, Copy, Default)]
pub struct StdFsProbe;

impl FsProbe for StdFsProbe {
    fn children(&self, dir: &str) -> Option<Vec<(String, bool)>> {
        let rd = std::fs::read_dir(dir).ok()?;
        Some(
            rd.filter_map(Result::ok)
                .map(|e| {
                    let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                    (e.file_name().to_string_lossy().into_owned(), is_dir)
                })
                .collect(),
        )
    }

    fn entry(&self, path: &str) -> Option<ProbeEntry> {
        let md = std::fs::symlink_metadata(path).ok()?;
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .map(FileTime::from_unix_secs);
        Some(ProbeEntry {
            is_dir: md.is_dir(),
            size: if md.is_dir() { 0 } else { md.len() },
            newest_mtime: mtime,
            flags: EntryFlags::EMPTY,
            magic: None,
        })
    }
}

/// Display form of the winning rule.
#[derive(Debug, Clone, Serialize)]
pub struct RuleSummary {
    /// Rule id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Pack id.
    pub pack: String,
    /// Built-in or user file.
    pub source: RuleSource,
    /// Explanation text.
    pub explain: String,
    /// Sub-category.
    pub subcategory: Option<String>,
    /// Action offered.
    pub action: Action,
    /// Tool offered.
    pub tool: Option<Tool>,
    /// Attribution label.
    pub app: Option<String>,
    /// Whether the data regenerates.
    pub regenerable: bool,
    /// The rule's own tier (the result may be stricter when clamped or
    /// combined with flags).
    pub safety: Safety,
}

/// One ancestor (or the entry itself) on the walk from the volume root.
#[derive(Debug, Clone, Serialize)]
pub struct ExplainStep {
    /// Path of this step.
    pub path: String,
    /// Its classification.
    pub classification: Classification,
    /// Winning rule id, if any.
    pub rule_id: Option<String>,
}

/// A full explanation of one path.
#[derive(Debug, Clone, Serialize)]
pub struct Explanation {
    /// The explained path.
    pub path: String,
    /// Final classification.
    pub result: Classification,
    /// The winning rule, if any.
    pub rule: Option<RuleSummary>,
    /// Path of the entry where the rule matched (an ancestor when inherited).
    pub origin_path: Option<String>,
    /// Every step from the first component below the volume root.
    pub steps: Vec<ExplainStep>,
    /// Candidate rules for the final entry and the decision taken.
    pub trace: Trace,
}

impl Classifier {
    /// Explains why `path` is classified the way it is.
    ///
    /// # Example
    ///
    /// ```
    /// use strata_classify::{Classifier, FsProbe, ProbeEntry, RuleSet};
    ///
    /// struct Fake;
    /// impl FsProbe for Fake {
    ///     fn children(&self, _: &str) -> Option<Vec<(String, bool)>> { Some(vec![]) }
    ///     fn entry(&self, p: &str) -> Option<ProbeEntry> {
    ///         Some(ProbeEntry { is_dir: !p.ends_with(".tmp"), ..Default::default() })
    ///     }
    /// }
    /// let c = Classifier::new(&RuleSet::builtin().unwrap(), &Default::default(), &Default::default()).unwrap();
    /// let e = c.explain(r"D:\work\build.tmp", &Fake);
    /// assert_eq!(e.rule.as_ref().unwrap().id, "generic.tmp_files");
    /// assert!(e.to_text(&c).contains("generic.tmp_files"));
    /// ```
    #[must_use]
    pub fn explain(&self, path: &str, probe: &dyn FsProbe) -> Explanation {
        let raw = path_components_raw(path);
        let mut trace = Trace::default();
        let mut steps = Vec::new();
        let Some(first) = raw.first() else {
            return self.finish(path, Classification::UNKNOWN, steps, trace, &raw);
        };
        let root_path = join_components(&raw[..1]);
        let kids = probe.children(&root_path).unwrap_or_default();
        let mut scope = self.root(
            first,
            kids.iter().map(|(n, d)| ChildRef {
                name: Name::Str(n),
                is_dir: *d,
            }),
        );
        let mut result = scope.classification();
        for i in 1..raw.len() {
            let here = join_components(&raw[..=i]);
            let last = i + 1 == raw.len();
            let facts = probe.entry(&here).unwrap_or(ProbeEntry {
                is_dir: !last,
                ..Default::default()
            });
            let mut entry = Entry {
                name: Name::Str(&raw[i]),
                flags: facts.flags,
                size: facts.size,
                newest_mtime: facts.newest_mtime,
                magic: facts.magic,
            };
            entry.flags.set(EntryFlags::DIR, facts.is_dir);
            if facts.is_dir {
                let kids = probe.children(&here);
                if kids.is_none() {
                    // Unreadable: the folder must not look empty.
                    entry.flags.set(EntryFlags::ACCESS_DENIED, true);
                }
                let kids = kids.unwrap_or_default();
                let refs = kids.iter().map(|(n, d)| ChildRef {
                    name: Name::Str(n),
                    is_dir: *d,
                });
                scope = self.enter_dir_inner(&scope, &entry, refs, last.then_some(&mut trace));
                result = scope.classification();
            } else {
                result = if last {
                    self.classify_file_traced(&scope, &entry, &mut trace)
                } else {
                    self.classify_file(&scope, &entry)
                };
            }
            steps.push(ExplainStep {
                path: here,
                classification: result,
                rule_id: result.rule.map(|r| self.rule(r).id.clone()),
            });
            if !facts.is_dir {
                break;
            }
        }
        self.finish(path, result, steps, trace, &raw)
    }

    fn finish(
        &self,
        path: &str,
        result: Classification,
        steps: Vec<ExplainStep>,
        trace: Trace,
        raw: &[String],
    ) -> Explanation {
        let rule = result.rule.map(|id| {
            let r = self.rule(id);
            RuleSummary {
                id: r.id.clone(),
                name: r.name.clone(),
                pack: r.pack.clone(),
                source: r.source.clone(),
                explain: r.explain.clone(),
                subcategory: r.subcategory.clone(),
                action: r.action,
                tool: r.tool,
                app: r.app.clone(),
                regenerable: r.regenerable,
                safety: r.safety,
            }
        });
        let origin_path = result.rule.map(|_| {
            let n = usize::from(result.origin_depth) + 1;
            join_components(&raw[..n.min(raw.len())])
        });
        Explanation {
            path: path.to_string(),
            result,
            rule,
            origin_path,
            steps,
            trace,
        }
    }
}

impl Explanation {
    /// Human-readable rendering for logs, tooltips and the CLI.
    #[must_use]
    pub fn to_text(&self, c: &Classifier) -> String {
        let mut s = String::new();
        let r = &self.result;
        let _ = writeln!(s, "{}", self.path);
        let _ = writeln!(
            s,
            "  Category: {}   Safety: {:?}",
            r.category.label(),
            r.safety
        );
        match &self.rule {
            None => {
                let _ = writeln!(
                    s,
                    "  No rule matches. Unknown data makes no safety claim and is treated as careful."
                );
            }
            Some(rule) => {
                let src = match &rule.source {
                    RuleSource::Builtin => format!("built-in pack `{}`", rule.pack),
                    RuleSource::User(f) => format!("user file `{f}`"),
                };
                let _ = writeln!(s, "  Rule: {} ({}) from {src}", rule.id, rule.name);
                let how = if r.inherited {
                    "inherited from"
                } else {
                    "matched at"
                };
                let _ = writeln!(
                    s,
                    "  {} {} by {}",
                    how,
                    self.origin_path.as_deref().unwrap_or("?"),
                    r.matched_by
                );
                let _ = writeln!(s, "  Why: {}", rule.explain);
                if let Some(app) = &rule.app {
                    let _ = writeln!(s, "  App: {app}");
                }
                if r.clamped {
                    let _ = writeln!(
                        s,
                        "  Safety raised to never: a built-in rule protects this location and user rules cannot relax it."
                    );
                }
            }
        }
        if !self.trace.candidates.is_empty() {
            let _ = writeln!(s, "  Candidates:");
            for cand in &self.trace.candidates {
                let rule = c.rule(cand.rule);
                let status = match cand.failed {
                    None => "matched".to_string(),
                    Some(m) => format!("failed `{m}`"),
                };
                let kind: MatchedBy = rule.matchers.primary();
                let _ = writeln!(s, "    - {} [{kind}, {:?}] {status}", rule.id, rule.safety);
            }
        }
        if let Some(d) = self.trace.decision {
            let text = match d {
                Decision::NoOwnMatch => "nothing matched this entry itself; it inherits",
                Decision::OwnMatch => "the best own match wins",
                Decision::OwnNeverWins => "an own `never` match always beats a less strict tier",
                Decision::InheritedHigherClass => {
                    "the inherited rule has higher precedence (path > name > extension)"
                }
                Decision::InheritedNeverNotCarvable => {
                    "the location is `never`; only explicit path rules can carve exceptions"
                }
            };
            let _ = writeln!(s, "  Decision: {text}");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::RuleSet;
    use crate::engine::DynamicRoots;
    use strata_core::known::{KnownFolder, KnownFolders};

    struct MapProbe(HashMap<String, Vec<(String, bool)>>);

    impl FsProbe for MapProbe {
        fn children(&self, dir: &str) -> Option<Vec<(String, bool)>> {
            self.0.get(&dir.to_ascii_uppercase()).cloned()
        }
        fn entry(&self, path: &str) -> Option<ProbeEntry> {
            let up = path.to_ascii_uppercase();
            let is_dir = self.0.contains_key(&up);
            Some(ProbeEntry {
                is_dir,
                ..Default::default()
            })
        }
    }

    #[test]
    fn explains_inherited_never() {
        let mut kf = KnownFolders::default();
        kf.machine.insert(KnownFolder::Windir, r"C:\Windows".into());
        let c =
            Classifier::new(&RuleSet::builtin().unwrap(), &kf, &DynamicRoots::default()).unwrap();
        let mut m = HashMap::new();
        m.insert(r"C:\".to_string(), vec![("Windows".to_string(), true)]);
        m.insert(
            r"C:\WINDOWS".to_string(),
            vec![("System32".to_string(), true)],
        );
        m.insert(
            r"C:\WINDOWS\SYSTEM32".to_string(),
            vec![("big.log".to_string(), false)],
        );
        let e = c.explain(r"C:\Windows\System32\big.log", &MapProbe(m));
        assert_eq!(e.result.safety, Safety::Never);
        assert!(e.result.inherited);
        assert_eq!(e.origin_path.as_deref(), Some(r"C:\Windows"));
        assert_eq!(e.steps.len(), 3);
        // The probe reports size 0, so `generic.zero_byte_file` matches, but
        // a non-path rule cannot relax the inherited `never`.
        assert_eq!(e.trace.decision, Some(Decision::InheritedNeverNotCarvable));
        assert!(e.trace.candidates.iter().any(|c| c.failed.is_none()));
        let text = e.to_text(&c);
        assert!(text.contains("inherited from C:\\Windows"), "{text}");
    }

    #[test]
    fn std_probe_reads_temp_dir() {
        let dir = std::env::temp_dir().join(format!("strata-explain-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("proj").join("node_modules")).unwrap();
        std::fs::write(dir.join("proj").join("package.json"), "{}").unwrap();
        let c = Classifier::new(
            &RuleSet::builtin().unwrap(),
            &KnownFolders::default(),
            &DynamicRoots::default(),
        )
        .unwrap();
        let p = dir.join("proj").join("node_modules");
        let e = c.explain(&p.to_string_lossy(), &StdFsProbe);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(e.rule.map(|r| r.id).as_deref(), Some("dev.node_modules"));
    }
}
