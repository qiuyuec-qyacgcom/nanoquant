//! 例外清单加载、预算校验、失效校验与豁免匹配。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{json, Value};

use super::scanner::ScanReport;

const SCHEMA_VERSION: &str = "1.0";
const BUDGET_TOTAL: usize = 12;
const BUDGET_PER_GRADE: usize = 6;
const ALLOWED_RULES: &[&str] = &["exec", "text", "naming", "artifact"];
const GRADES: &[&str] = &["exec", "text", "naming", "artifact"];

#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub code: String,
    pub scopes: Vec<String>,
    pub rules: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Registry {
    pub entries: Vec<Entry>,
    pub total: usize,
    pub by_grade: BTreeMap<String, usize>,
    pub invalid: Vec<String>,
}

impl Registry {
    pub(crate) fn is_valid(&self) -> bool {
        self.invalid.is_empty()
    }

    pub(crate) fn waive(&self, file: &str, grade: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| {
                entry.rules.iter().any(|rule| rule == grade)
                    && entry.scopes.iter().any(|scope| scope_matches(scope, file))
            })
            .map(|entry| entry.code.as_str())
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "total": self.total,
            "by_grade": self.by_grade,
            "invalid": self.invalid,
        })
    }
}

pub(crate) fn apply(registry: &Registry, report: &mut ScanReport) {
    for hit in &mut report.hits {
        if hit.waived_by.is_none() {
            hit.waived_by = registry
                .waive(&hit.file, hit.grade)
                .map(ToString::to_string);
        }
    }
}

pub(crate) fn load(root: &Path, path: &Path) -> Registry {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    match std::fs::read_to_string(&absolute) {
        Ok(text) => load_document(root, &text),
        Err(error) => {
            let mut registry = Registry::default();
            registry
                .invalid
                .push(format!("file.unreadable: {} ({error})", path.display()));
            registry
        }
    }
}

pub(crate) fn load_document(root: &Path, text: &str) -> Registry {
    let mut registry = Registry::default();
    let document: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            registry.invalid.push(format!("json.parse: {error}"));
            return registry;
        }
    };
    if document.get("schema_version").and_then(Value::as_str) != Some(SCHEMA_VERSION) {
        registry.invalid.push(format!(
            "schema_version.mismatch: expected {SCHEMA_VERSION}"
        ));
    }
    match document.pointer("/budget/total").and_then(Value::as_u64) {
        Some(value) if value as usize <= BUDGET_TOTAL => {}
        Some(value) => registry
            .invalid
            .push(format!("budget.total: {value} > {BUDGET_TOTAL}")),
        None => registry.invalid.push("budget.total: missing".to_string()),
    }
    match document
        .pointer("/budget/per_grade")
        .and_then(Value::as_u64)
    {
        Some(value) if value as usize <= BUDGET_PER_GRADE => {}
        Some(value) => registry
            .invalid
            .push(format!("budget.per_grade: {value} > {BUDGET_PER_GRADE}")),
        None => registry
            .invalid
            .push("budget.per_grade: missing".to_string()),
    }
    let Some(entries) = document.get("entries").and_then(Value::as_array) else {
        registry.invalid.push("entries: missing".to_string());
        return registry;
    };
    let mut codes = BTreeSet::new();
    for (index, raw) in entries.iter().enumerate() {
        if let Some(entry) = parse_entry(root, raw, index, &mut codes, &mut registry) {
            registry.entries.push(entry);
        }
    }
    registry.total = registry.entries.len();
    for entry in &registry.entries {
        for rule in &entry.rules {
            *registry.by_grade.entry(rule.clone()).or_insert(0) += 1;
        }
    }
    if registry.total > BUDGET_TOTAL {
        registry
            .invalid
            .push(format!("budget.total: {} > {BUDGET_TOTAL}", registry.total));
    }
    for (grade, count) in &registry.by_grade {
        if *count > BUDGET_PER_GRADE {
            registry.invalid.push(format!(
                "budget.per_grade: {grade}={count} > {BUDGET_PER_GRADE}"
            ));
        }
    }
    for grade in GRADES {
        if registry.by_grade.get(*grade).copied().unwrap_or(0) < 1 {
            registry.invalid.push(format!("grade.{grade}: coverage<1"));
        }
    }
    registry
}

fn parse_entry(
    root: &Path,
    raw: &Value,
    index: usize,
    codes: &mut BTreeSet<String>,
    registry: &mut Registry,
) -> Option<Entry> {
    let code = raw
        .get("code")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let label = if code.is_empty() {
        format!("entries[{index}]")
    } else {
        code.clone()
    };
    let mut ok = true;
    if !is_ex_code(&code) {
        registry.invalid.push(format!("{label}: code.invalid"));
        ok = false;
    } else if !codes.insert(code.clone()) {
        registry.invalid.push(format!("{label}: duplicate.code"));
        ok = false;
    }

    let mut scopes = Vec::new();
    match raw.get("scopes").and_then(Value::as_array) {
        Some(items) if !items.is_empty() => {
            for item in items {
                let Some(scope) = item
                    .as_str()
                    .map(str::trim)
                    .filter(|scope| !scope.is_empty())
                else {
                    registry.invalid.push(format!("{label}: scope.empty"));
                    ok = false;
                    continue;
                };
                if Path::new(scope).is_absolute() {
                    registry
                        .invalid
                        .push(format!("{label}: scope.absolute: {scope}"));
                    ok = false;
                } else if has_parent_component(scope) {
                    registry
                        .invalid
                        .push(format!("{label}: scope.unsafe: {scope}"));
                    ok = false;
                } else {
                    scopes.push(scope.replace('\\', "/"));
                }
            }
        }
        _ => {
            registry.invalid.push(format!("{label}: scopes.missing"));
            ok = false;
        }
    }

    let mut rules = Vec::new();
    match raw.get("rules").and_then(Value::as_array) {
        Some(items) if !items.is_empty() => {
            for item in items {
                let Some(rule) = item.as_str().map(str::trim) else {
                    registry.invalid.push(format!("{label}: rule.empty"));
                    ok = false;
                    continue;
                };
                if !ALLOWED_RULES.contains(&rule) {
                    registry
                        .invalid
                        .push(format!("{label}: rule.unknown: {rule}"));
                    ok = false;
                } else {
                    rules.push(rule.to_string());
                }
            }
        }
        _ => {
            registry.invalid.push(format!("{label}: rules.missing"));
            ok = false;
        }
    }

    for field in ["reason", "owner", "expires_when"] {
        if string_field(Some(raw), field).is_none() {
            registry
                .invalid
                .push(format!("{label}: field.empty: {field}"));
            ok = false;
        }
    }

    let verify_kind = string_field(raw.pointer("/verify"), "kind");
    let verify_target = string_field(raw.pointer("/verify"), "target");
    let needle = string_field(raw.pointer("/verify"), "needle");
    match verify_kind.as_deref() {
        Some("path_exists") => {}
        Some("contains") => {
            if needle.is_none() {
                registry
                    .invalid
                    .push(format!("{label}: verify.needle: missing"));
                ok = false;
            }
        }
        Some(other) => {
            registry
                .invalid
                .push(format!("{label}: verify.kind: {other}"));
            ok = false;
        }
        None => {
            registry
                .invalid
                .push(format!("{label}: verify.kind: missing"));
            ok = false;
        }
    }
    match verify_target.as_deref() {
        Some(target) if !Path::new(target).is_absolute() && !has_parent_component(target) => {
            if let Err(reason) = verify(root, verify_kind.as_deref(), target, needle.as_deref()) {
                registry
                    .invalid
                    .push(format!("{label}: verify.failed: {reason}"));
            }
        }
        Some(target) => {
            registry
                .invalid
                .push(format!("{label}: verify.target.unsafe: {target}"));
            ok = false;
        }
        None => {
            registry
                .invalid
                .push(format!("{label}: verify.target: missing"));
            ok = false;
        }
    }

    if !ok {
        return None;
    }
    Some(Entry {
        code,
        scopes,
        rules,
    })
}

fn verify(
    root: &Path,
    kind: Option<&str>,
    target: &str,
    needle: Option<&str>,
) -> Result<(), String> {
    let path = root.join(target);
    match kind {
        Some("path_exists") => {
            if path.exists() {
                Ok(())
            } else {
                Err(format!("path missing: {target}"))
            }
        }
        Some("contains") => {
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("read {target}: {error}"))?;
            if text.contains(needle.unwrap_or_default()) {
                Ok(())
            } else {
                Err(format!("keyword missing in {target}"))
            }
        }
        _ => Err("unknown verify kind".to_string()),
    }
}

fn string_field(value: Option<&Value>, key: &str) -> Option<String> {
    value
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
}

fn is_ex_code(code: &str) -> bool {
    let Some(rest) = code.strip_prefix("EX-") else {
        return false;
    };
    rest.len() >= 2 && rest.chars().all(|item| item.is_ascii_digit())
}

fn has_parent_component(value: &str) -> bool {
    Path::new(value)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}

fn scope_matches(scope: &str, file: &str) -> bool {
    let scope = scope.trim_end_matches('/');
    if scope.is_empty() {
        return false;
    }
    file.strip_prefix(scope)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nq-interpreter-refs-exc-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.md"), "README\n").unwrap();
        dir
    }

    fn entry(code: &str, scopes: &[&str], rules: &[&str]) -> Value {
        json!({
            "code": code,
            "scopes": scopes,
            "rules": rules,
            "reason": "门禁实现必须保留。",
            "owner": "维护者",
            "expires_when": "门禁下线时",
            "verify": { "kind": "path_exists", "target": "README.md" }
        })
    }

    fn document(entries: Vec<Value>) -> String {
        json!({
            "schema_version": "1.0",
            "budget": { "total": 12, "per_grade": 6 },
            "entries": entries
        })
        .to_string()
    }

    fn four_grades() -> Vec<Value> {
        vec![
            entry("EX-01", &["README.md"], &["exec"]),
            entry("EX-02", &["README.md"], &["text"]),
            entry("EX-03", &["README.md"], &["naming"]),
            entry("EX-04", &["README.md"], &["artifact"]),
        ]
    }

    #[test]
    fn loads_committed_registry_with_expected_grade_counts() {
        let root = repo_root();
        let text =
            std::fs::read_to_string(root.join(".github/ci/python-refs-exceptions.json")).unwrap();
        let registry = load_document(&root, &text);
        assert!(
            registry
                .invalid
                .iter()
                .all(|item| item.contains("verify.failed")),
            "{:?}",
            registry.invalid
        );
        assert_eq!(registry.total, 7);
        assert_eq!(registry.by_grade.get("exec"), Some(&1));
        assert_eq!(registry.by_grade.get("text"), Some(&6));
        assert_eq!(registry.by_grade.get("naming"), Some(&3));
        assert_eq!(registry.by_grade.get("artifact"), Some(&1));
    }

    #[test]
    fn missing_file_is_invalid_not_skipped() {
        let dir = workspace("missing");
        let registry = load(&dir, Path::new(".github/ci/python-refs-exceptions.json"));
        assert!(!registry.invalid.is_empty());
        assert_eq!(registry.total, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn total_budget_overflow_is_invalid() {
        let dir = workspace("total");
        let mut entries = vec![
            entry("EX-01", &["README.md"], &["exec"]),
            entry("EX-02", &["README.md"], &["naming"]),
            entry("EX-03", &["README.md"], &["artifact"]),
        ];
        for index in 0..10 {
            entries.push(entry(
                &format!("EX-{:02}", index + 4),
                &["README.md"],
                &["text"],
            ));
        }
        let registry = load_document(&dir, &document(entries));
        assert!(registry
            .invalid
            .iter()
            .any(|item| item.contains("budget.total")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_grade_budget_overflow_is_invalid() {
        let dir = workspace("per-grade");
        let mut entries = vec![
            entry("EX-01", &["README.md"], &["exec"]),
            entry("EX-02", &["README.md"], &["naming"]),
            entry("EX-03", &["README.md"], &["artifact"]),
        ];
        for index in 0..7 {
            entries.push(entry(
                &format!("EX-{:02}", index + 4),
                &["README.md"],
                &["text"],
            ));
        }
        let registry = load_document(&dir, &document(entries));
        assert!(registry
            .invalid
            .iter()
            .any(|item| item.contains("budget.per_grade")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_grade_must_have_at_least_one_entry() {
        let dir = workspace("coverage");
        let entries = vec![
            entry("EX-01", &["README.md"], &["exec"]),
            entry("EX-02", &["README.md"], &["text"]),
            entry("EX-03", &["README.md"], &["naming"]),
        ];
        let registry = load_document(&dir, &document(entries));
        assert!(registry
            .invalid
            .iter()
            .any(|item| item.contains("grade.artifact")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_rule_and_duplicate_code_are_invalid() {
        let dir = workspace("rules");
        let mut entries = four_grades();
        entries.push(entry("EX-01", &["README.md"], &["docs"]));
        let registry = load_document(&dir, &document(entries));
        assert!(registry
            .invalid
            .iter()
            .any(|item| item.contains("duplicate.code")));
        assert!(registry
            .invalid
            .iter()
            .any(|item| item.contains("rule.unknown") || item.contains("rule.docs")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absolute_or_parent_scope_is_invalid() {
        let dir = workspace("scope");
        let mut entries = four_grades();
        entries.push(entry("EX-05", &["../escape.md"], &["text"]));
        entries.push(entry("EX-06", &["C:/abs.md"], &["text"]));
        let registry = load_document(&dir, &document(entries));
        assert!(registry
            .invalid
            .iter()
            .any(|item| item.contains("scope.unsafe")));
        assert!(registry
            .invalid
            .iter()
            .any(|item| item.contains("scope.absolute")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_verify_is_invalid() {
        let dir = workspace("verify");
        let mut entries = four_grades();
        entries.push(json!({
            "code": "EX-05",
            "scopes": ["README.md"],
            "rules": ["text"],
            "reason": "证明失效校验。",
            "owner": "维护者",
            "expires_when": "文件出现时",
            "verify": { "kind": "path_exists", "target": "missing.md" }
        }));
        let registry = load_document(&dir, &document(entries));
        assert!(registry
            .invalid
            .iter()
            .any(|item| item.contains("EX-05") && item.contains("verify")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn waiver_matches_first_entry_by_scope_and_grade() {
        let dir = workspace("waive");
        let mut entries = four_grades();
        entries.push(entry("EX-05", &["docs/"], &["text"]));
        entries.push(entry("EX-06", &["docs/"], &["text"]));
        let registry = load_document(&dir, &document(entries));
        assert!(registry.invalid.is_empty(), "{:?}", registry.invalid);
        assert_eq!(registry.waive("docs/a.md", "text"), Some("EX-05"));
        assert_eq!(registry.waive("docs/a.md", "exec"), None);
        assert_eq!(registry.waive("src/a.md", "text"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gate_selftest_markers_exist() {
        let root = repo_root();
        let cli = std::fs::read_to_string(root.join("rust/nanoquant/src/cli.rs")).unwrap();
        assert!(cli.contains("fn zero_python"));
        assert!(cli.contains("fn python_processes"));
        assert!(cli.contains("NANOQUANT_ROLLBACK_KERNEL"));
        let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
        assert!(ci.contains("gate (zero-python repo)"));
        assert!(ci.contains("gate (zero-python runtime)"));
        assert!(root.join(".github/ci/zero_python_gate.sh").exists());
        let allowlist =
            std::fs::read_to_string(root.join(".github/ci/zero-python-allowlist.txt")).unwrap();
        assert!(allowlist
            .lines()
            .next()
            .is_some_and(|line| line.starts_with('#')));
    }
}
