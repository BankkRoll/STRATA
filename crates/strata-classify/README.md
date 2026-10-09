# strata-classify

Answers "what is this data, who owns it, and is it safe to remove?" for every file and folder,
using TOML rule packs. The rule engine never calls Win32; it takes resolved known folders.

## Responsibilities

- `schema` / `pack`: the rule-pack format, the built-in packs embedded from
  [`rules/`](../../rules), user packs that override them (`RuleSet::builtin`, `RuleSet::load`),
  and the guarantee that built-in `never` rules cannot be relaxed.
- `engine`: the compiled `Classifier` and its top-down tree-walk API (`Classifier::root`,
  `enter_dir`, `classify_file`), producing a category, safety tier and owning app.
- `explain`: "why is this classified as X?" (`Classifier::explain`).
- `catalog`: installed-apps catalog and app attribution (registry, AppX).
- `discover`: runtime roots for rules (Steam libraries, OBS recordings, WSL distros, ...).
- `sniff`: content-type detection from file headers.
- `fold`: Windows-correct case folding and path normalization.

## Test

```powershell
cargo test -p strata-classify
cargo run --release -p strata-classify --example machine_report   # classify this machine
```

Every built-in rule must have a fixture case in `tests/fixtures/rules.toml`; the tests fail
otherwise.

## See also

- Rule schema and authoring guide: [docs/RULES.md](../../docs/RULES.md)
