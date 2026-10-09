//! Loading built-in and user rule packs into one [`RuleSet`].
//!
//! Override semantics (see "Overrides and safety policy" in `docs/RULES.md`):
//! - A user rule with the id of a built-in rule replaces it.
//! - A user pack may `disable` built-in rules by id.
//! - A built-in `never` rule can be neither relaxed (overridden with a lower
//!   tier) nor disabled. Such attempts are reported and ignored: the never
//!   tier protects the OS and user data from deletion, and a rules file must
//!   not be able to remove that protection.
//! - Errors in user packs never prevent the built-ins from loading; they are
//!   collected in a [`LoadReport`]. Errors in built-in packs are bugs and fail
//!   the load.

use std::collections::BTreeMap;
use std::path::Path;

use rustc_hash::FxHashMap;
use strata_core::Safety;

use crate::builtin::BUILTIN_PACKS;
use crate::schema::{PackFile, Rule, RuleSource, SchemaError, parse_pack, validate_rule};

/// Problems and notes from loading packs.
#[derive(Debug, Clone, Default)]
pub struct LoadReport {
    /// Rejected rules, packs and override attempts. Each was skipped.
    pub errors: Vec<SchemaError>,
    /// Non-fatal notes (unknown ids in `disable`, user rules redefined by a
    /// later file).
    pub warnings: Vec<String>,
    /// Built-in rule ids replaced by user rules.
    pub overridden: Vec<String>,
    /// Built-in rule ids disabled by user packs.
    pub disabled: Vec<String>,
}

/// A validated, override-resolved set of rules, ready to compile into a
/// [`crate::Classifier`].
#[derive(Debug, Clone)]
pub struct RuleSet {
    rules: Vec<Rule>,
    report: LoadReport,
}

/// A user pack's file name and TOML text.
#[derive(Debug, Clone)]
pub struct UserPack {
    /// File name, for messages.
    pub file: String,
    /// TOML text.
    pub text: String,
}

impl RuleSet {
    /// Loads the built-in packs only.
    ///
    /// # Errors
    ///
    /// Returns every validation error in the built-in packs (a bug).
    ///
    /// # Example
    ///
    /// ```
    /// let rules = strata_classify::RuleSet::builtin().unwrap();
    /// assert!(rules.rules().iter().any(|r| r.id == "dev.node_modules"));
    /// ```
    pub fn builtin() -> Result<Self, Vec<SchemaError>> {
        Self::load(&[])
    }

    /// Loads the built-in packs plus user packs, applying overrides.
    ///
    /// # Errors
    ///
    /// Fails only if a built-in pack is invalid. User-pack problems are in
    /// [`RuleSet::report`].
    pub fn load(user: &[UserPack]) -> Result<Self, Vec<SchemaError>> {
        let builtin: Vec<(&str, &str)> = BUILTIN_PACKS.to_vec();
        Self::load_from(&builtin, user)
    }

    /// Loads explicit built-in pack texts (used by tests and tooling).
    ///
    /// # Errors
    ///
    /// Fails if any of `builtin` is invalid.
    pub fn load_from(
        builtin: &[(&str, &str)],
        user: &[UserPack],
    ) -> Result<Self, Vec<SchemaError>> {
        let mut errors = Vec::new();
        let mut packs: Vec<(String, PackFile)> = Vec::new();
        for (file, text) in builtin {
            match parse_pack(file, text) {
                Ok(p) => packs.push(((*file).to_string(), p)),
                Err(e) => errors.push(e),
            }
        }
        let mut lists: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (file, p) in &packs {
            for (name, values) in &p.lists {
                if lists.insert(name.clone(), values.clone()).is_some() {
                    errors.push(SchemaError::Pack {
                        file: file.clone(),
                        message: format!("list `{name}` is defined twice"),
                    });
                }
            }
            if !p.disable.is_empty() {
                errors.push(SchemaError::Pack {
                    file: file.clone(),
                    message: "built-in packs cannot use `disable`".into(),
                });
            }
        }
        let mut rules: Vec<Rule> = Vec::new();
        let mut by_id: FxHashMap<String, usize> = FxHashMap::default();
        for (file, p) in packs {
            for raw in p.rules {
                let id = raw.id.clone();
                match validate_rule(raw, &p.pack, &RuleSource::Builtin, &lists) {
                    Ok(rule) => {
                        if by_id.insert(rule.id.clone(), rules.len()).is_some() {
                            errors.push(SchemaError::Rule {
                                file: file.clone(),
                                id,
                                message: "duplicate id".into(),
                            });
                        } else {
                            rules.push(rule);
                        }
                    }
                    Err(message) => errors.push(SchemaError::Rule {
                        file: file.clone(),
                        id,
                        message,
                    }),
                }
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }

        let mut report = LoadReport::default();
        let mut user_packs: Vec<(String, PackFile)> = Vec::new();
        for up in user {
            match parse_pack(&up.file, &up.text) {
                Ok(p) => user_packs.push((up.file.clone(), p)),
                Err(e) => report.errors.push(e),
            }
        }
        for (file, p) in &user_packs {
            for (name, values) in &p.lists {
                if lists.contains_key(name) {
                    report.errors.push(SchemaError::Pack {
                        file: file.clone(),
                        message: format!("list `{name}` already exists; list names are global"),
                    });
                } else {
                    lists.insert(name.clone(), values.clone());
                }
            }
        }
        let builtin_count = rules.len();
        let mut removed = vec![false; builtin_count];
        for (file, p) in user_packs {
            let source = RuleSource::User(file.clone());
            for id in &p.disable {
                match by_id.get(id) {
                    Some(&i) if i < builtin_count => {
                        if rules[i].safety == Safety::Never {
                            report.errors.push(SchemaError::Rule {
                                file: file.clone(),
                                id: id.clone(),
                                message: "built-in `never` rules cannot be disabled".into(),
                            });
                        } else {
                            removed[i] = true;
                            report.disabled.push(id.clone());
                        }
                    }
                    _ => report.warnings.push(format!(
                        "{file}: `disable` names unknown built-in rule `{id}`"
                    )),
                }
            }
            for raw in p.rules {
                let id = raw.id.clone();
                let rule = match validate_rule(raw, &p.pack, &source, &lists) {
                    Ok(r) => r,
                    Err(message) => {
                        report.errors.push(SchemaError::Rule {
                            file: file.clone(),
                            id,
                            message,
                        });
                        continue;
                    }
                };
                match by_id.get(&rule.id).copied() {
                    Some(i) if i < builtin_count => {
                        if rules[i].safety == Safety::Never && rule.safety != Safety::Never {
                            report.errors.push(SchemaError::Rule {
                                file: file.clone(),
                                id,
                                message: "cannot relax a built-in `never` rule".into(),
                            });
                            continue;
                        }
                        if removed[i] {
                            report.warnings.push(format!(
                                "{file}: rule `{id}` both disabled and overridden; the override wins"
                            ));
                            removed[i] = false;
                            report.disabled.retain(|d| d != &id);
                        }
                        rules[i] = rule;
                        report.overridden.push(id);
                    }
                    Some(i) => {
                        report.warnings.push(format!(
                            "{file}: user rule `{id}` redefined; the later file wins"
                        ));
                        rules[i] = rule;
                    }
                    None => {
                        by_id.insert(rule.id.clone(), rules.len());
                        rules.push(rule);
                    }
                }
            }
        }
        let rules = rules
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !removed.get(*i).copied().unwrap_or(false))
            .map(|(_, r)| r)
            .collect();
        Ok(Self { rules, report })
    }

    /// The effective rules.
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// What happened while loading user packs.
    #[must_use]
    pub fn report(&self) -> &LoadReport {
        &self.report
    }

    /// Whether any user rule is active.
    #[must_use]
    pub fn has_user_rules(&self) -> bool {
        self.rules.iter().any(|r| !r.is_builtin())
    }
}

/// Reads every `*.toml` file in a user rules folder, sorted by file name so
/// later files win deterministically.
///
/// A missing folder yields no packs.
///
/// # Errors
///
/// Returns I/O errors other than "not found".
pub fn read_user_dir(dir: &Path) -> std::io::Result<Vec<UserPack>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("toml"))
        })
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|p| {
            Ok(UserPack {
                file: p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                text: std::fs::read_to_string(&p)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"schema_version = 1
pack = "b"
[lists]
names = ["a", "b"]
[[rule]]
id = "b.never"
name = "N"
category = "system"
safety = "never"
explain = "x"
match.path = "{WINDIR}"
[[rule]]
id = "b.safe"
name = "S"
category = "caches"
safety = "safe"
explain = "x"
match.dir_name = "@names"
"#;

    fn user(text: &str) -> UserPack {
        UserPack {
            file: "u.toml".into(),
            text: format!("schema_version = 1\npack = \"u\"\n{text}"),
        }
    }

    #[test]
    fn overrides_and_disables() {
        let rs = RuleSet::load_from(
            &[("b.toml", BASE)],
            &[user(
                r#"disable = ["b.safe", "b.nope"]
[[rule]]
id = "u.mine"
name = "Mine"
category = "temp"
safety = "probably"
explain = "mine"
match.dir_name = "@names"
"#,
            )],
        )
        .unwrap();
        let ids: Vec<_> = rs.rules().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["b.never", "u.mine"]);
        assert_eq!(rs.report().disabled, ["b.safe"]);
        assert_eq!(rs.report().warnings.len(), 1);
        assert!(rs.has_user_rules());
    }

    #[test]
    fn never_cannot_be_relaxed_or_disabled() {
        let rs = RuleSet::load_from(
            &[("b.toml", BASE)],
            &[user(
                r#"disable = ["b.never"]
[[rule]]
id = "b.never"
name = "Relaxed"
category = "system"
safety = "safe"
explain = "nope"
match.path = "{WINDIR}"
"#,
            )],
        )
        .unwrap();
        let never = rs.rules().iter().find(|r| r.id == "b.never").unwrap();
        assert_eq!(never.safety, Safety::Never);
        assert!(never.is_builtin());
        assert_eq!(rs.report().errors.len(), 2);
    }

    #[test]
    fn never_may_be_reworded() {
        let rs = RuleSet::load_from(
            &[("b.toml", BASE)],
            &[user(
                r#"[[rule]]
id = "b.never"
name = "Windows (mine)"
category = "system"
safety = "never"
explain = "custom text"
match.path = "{WINDIR}"
"#,
            )],
        )
        .unwrap();
        let never = rs.rules().iter().find(|r| r.id == "b.never").unwrap();
        assert_eq!(never.explain, "custom text");
        assert_eq!(rs.report().overridden, ["b.never"]);
    }

    #[test]
    fn bad_user_pack_does_not_block_builtins() {
        let rs = RuleSet::load_from(
            &[("b.toml", BASE)],
            &[
                UserPack {
                    file: "x.toml".into(),
                    text: "not toml [".into(),
                },
                user("[lists]\nnames = [\"dup\"]"),
            ],
        )
        .unwrap();
        assert_eq!(rs.rules().len(), 2);
        assert_eq!(rs.report().errors.len(), 2);
    }

    #[test]
    fn builtin_errors_fail() {
        let bad = BASE.replace("safety = \"safe\"", "safety = \"sorta\"");
        assert!(RuleSet::load_from(&[("b.toml", &bad)], &[]).is_err());
        let dup = format!(
            "{BASE}\n[[rule]]\nid = \"b.safe\"\nname = \"S\"\ncategory = \"caches\"\nsafety = \"safe\"\nexplain = \"x\"\nmatch.ext = \"x\"\n"
        );
        assert!(RuleSet::load_from(&[("b.toml", &dup)], &[]).is_err());
    }

    #[test]
    fn reads_user_dir() {
        let dir = std::env::temp_dir().join(format!("strata-classify-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.toml"), "schema_version = 1\npack = \"b\"").unwrap();
        std::fs::write(dir.join("a.TOML"), "schema_version = 1\npack = \"a\"").unwrap();
        std::fs::write(dir.join("readme.txt"), "x").unwrap();
        let packs = read_user_dir(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let names: Vec<_> = packs.iter().map(|p| p.file.as_str()).collect();
        assert_eq!(names, ["a.TOML", "b.toml"]);
        assert!(read_user_dir(&dir).unwrap().is_empty());
    }
}
