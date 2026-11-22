use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::rules::{self, Grade, Rule};

pub(crate) const DEFAULT_MAX_DEPTH: usize = 32;
pub(crate) const DEFAULT_MAX_HITS: usize = 5_000;
pub(crate) const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const EXCLUSIONS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "data",
    ".codeartsdoer",
    ".superpowers",
    ".codebuddy",
];

#[derive(Debug, Clone)]
pub(crate) struct ScanOptions {
    pub root: PathBuf,
    pub max_depth: usize,
    pub max_hits: usize,
    pub max_file_bytes: u64,
    pub archive_root: Option<PathBuf>,
}

impl ScanOptions {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_depth: DEFAULT_MAX_DEPTH,
            max_hits: DEFAULT_MAX_HITS,
            max_file_bytes: MAX_TEXT_BYTES,
            archive_root: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Hit {
    pub file: String,
    pub line: usize,
    pub rule: &'static str,
    pub grade: &'static str,
    /// 命中规则的人话说明：只给 code 看不出这条到底管什么。
    pub description: &'static str,
    pub matched_rules: Vec<&'static str>,
    pub snippet_hash: String,
    pub waived_by: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct ScanReport {
    pub hits: Vec<Hit>,
    pub excluded: Vec<String>,
    pub unreadable: Vec<String>,
    pub skipped_large: Vec<SkippedLarge>,
    pub rejected: Vec<String>,
    pub skipped_missing: Vec<String>,
    pub truncated: bool,
    pub graded: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SkippedLarge {
    pub file: String,
    pub bytes: u64,
}

pub(crate) fn scan(options: &ScanOptions) -> ScanReport {
    let mut report = ScanReport::default();
    if !options.root.is_dir() {
        report.unreadable.push(display_path(&options.root));
        return report;
    }
    walk(options, &options.root, &options.root, 0, &mut report);
    finalize(&mut report);
    report
}

pub(crate) fn scan_changed_files(options: &ScanOptions, list: &Path) -> ScanReport {
    let mut report = ScanReport::default();
    if !options.root.is_dir() {
        report.unreadable.push(display_path(&options.root));
        return report;
    }
    let content = match std::fs::read_to_string(list) {
        Ok(content) => content,
        Err(_) => {
            report.unreadable.push(display_path(list));
            finalize(&mut report);
            return report;
        }
    };
    let mut seen = std::collections::BTreeSet::new();
    for raw in content.lines() {
        let entry = raw.trim();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        let relative_entry = entry.replace('\\', "/");
        let path = Path::new(entry);
        if path.is_absolute() || has_parent_component(path) {
            report.rejected.push(relative_entry);
            continue;
        }
        if !seen.insert(relative_entry.clone()) {
            continue;
        }
        let absolute = options.root.join(path);
        if !absolute.starts_with(&options.root) {
            report.rejected.push(relative_entry);
            continue;
        }
        let is_file = std::fs::metadata(&absolute)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false);
        if !is_file {
            report.skipped_missing.push(relative_entry);
            continue;
        }
        let Some(name) = absolute
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
        else {
            report.skipped_missing.push(relative_entry);
            continue;
        };
        if let Some((rule, grade)) = file_rule(&name) {
            push_hit(
                &options.root,
                &absolute,
                0,
                rule,
                grade,
                vec![rule],
                "",
                &mut report,
            );
            if grade == Grade::Exec && name.to_lowercase().ends_with(".py") {
                continue;
            }
        }
        if is_text_file(&name) {
            scan_text(options, &options.root, &absolute, &mut report);
        }
    }
    finalize(&mut report);
    report
}

pub(crate) fn out_path_is_allowed(root: &Path, out: &Path) -> bool {
    if has_parent_component(out) {
        return false;
    }
    let root = absolute_root(root);
    let candidate = if out.is_absolute() {
        out.to_path_buf()
    } else {
        root.join(out)
    };
    let Ok(relative) = candidate.strip_prefix(&root) else {
        return true;
    };
    let Some(first) = relative.components().next() else {
        return false;
    };
    let first = first.as_os_str().to_string_lossy();
    EXCLUSIONS.iter().any(|item| *item == first)
}

fn absolute_root(root: &Path) -> PathBuf {
    if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(root))
            .unwrap_or_else(|_| root.to_path_buf())
    }
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}

fn finalize(report: &mut ScanReport) {
    report.hits.sort_by(|left, right| {
        (&left.file, left.line, left.rule, left.grade).cmp(&(
            &right.file,
            right.line,
            right.rule,
            right.grade,
        ))
    });
    for grade in ["exec", "text", "naming", "artifact"] {
        let count = report.hits.iter().filter(|hit| hit.grade == grade).count();
        report.graded.insert(grade.to_string(), count);
    }
}

fn walk(options: &ScanOptions, root: &Path, dir: &Path, depth: usize, report: &mut ScanReport) {
    if report.hits.len() >= options.max_hits {
        report.truncated = true;
        return;
    }
    if depth > options.max_depth {
        report.truncated = true;
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        report.unreadable.push(relative(root, dir));
        return;
    };
    let mut entries = read_dir.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if EXCLUSIONS.contains(&name.as_str()) {
            report.excluded.push(relative(root, &path));
            continue;
        }
        if path.is_dir() {
            if matches!(name.as_str(), ".venv" | "__pycache__") {
                push_file_hit(root, &path, "exec.python_env", report);
                continue;
            }
            walk(options, root, &path, depth + 1, report);
            continue;
        }
        if let Some((rule, grade)) = file_rule(&name) {
            push_hit(root, &path, 0, rule, grade, vec![rule], "", report);
            if grade == Grade::Exec && name.ends_with(".py") {
                continue;
            }
        }
        if !is_text_file(&name) {
            continue;
        }
        scan_text(options, root, &path, report);
    }
}

fn file_rule(name: &str) -> Option<(&'static str, Grade)> {
    let lower = name.to_lowercase();
    if lower.ends_with(".py") {
        Some(("exec.python_script", Grade::Exec))
    } else if lower.starts_with("requirements") && lower.ends_with(".txt") {
        Some(("exec.requirements", Grade::Exec))
    } else if matches!(
        lower.as_str(),
        "pyproject.toml" | "ruff.toml" | "mypy.ini" | ".coveragerc"
    ) {
        Some(("exec.dep_tool", Grade::Exec))
    } else if lower.starts_with("zero-python-") && lower.ends_with(".json") {
        Some(("artifact.report_json", Grade::Artifact))
    } else if lower.starts_with("target-") && lower.ends_with(".txt") {
        Some(("artifact.target_dump", Grade::Artifact))
    } else if lower.ends_with(".py.bak")
        || lower.ends_with(".py.txt")
        || lower.ends_with(".py.example")
    {
        Some(("artifact.backup_copy", Grade::Artifact))
    } else {
        None
    }
}

fn is_text_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    [
        ".sh", ".ps1", ".yml", ".yaml", ".service", ".toml", ".cfg", ".ini", ".md", ".rs", ".ts",
        ".vue", ".json", ".txt", ".html", ".css", ".js",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
        || lower.starts_with("dockerfile")
        || lower.starts_with("docker-compose")
        || lower == "makefile"
}

fn scan_text(options: &ScanOptions, root: &Path, path: &Path, report: &mut ScanReport) {
    let relative_path = relative(root, path);
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => {
            report.unreadable.push(relative_path);
            return;
        }
    };
    if metadata.len() > options.max_file_bytes {
        report.skipped_large.push(SkippedLarge {
            file: relative_path,
            bytes: metadata.len(),
        });
        return;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => {
            report.unreadable.push(relative_path);
            return;
        }
    };
    let in_archive = options
        .archive_root
        .as_deref()
        .is_some_and(|archive| path.starts_with(archive));
    for (index, line) in text.lines().enumerate() {
        if in_archive && line.trim_start().starts_with("# [已删除]") {
            continue;
        }
        if report.hits.len() >= options.max_hits {
            report.truncated = true;
            return;
        }
        if !rules::coarse(line) {
            continue;
        }
        let matched: Vec<&Rule> = rules::rules()
            .iter()
            .filter(|rule| rule.grade != Grade::Docs && rules::matches_line(rule, line))
            .collect();
        let Some(main) = matched.iter().min_by_key(|rule| rule.grade) else {
            continue;
        };
        push_hit(
            root,
            path,
            index + 1,
            main.code,
            main.grade,
            matched.iter().map(|rule| rule.code).collect(),
            line,
            report,
        );
    }
}

fn push_file_hit(root: &Path, path: &Path, rule: &'static str, report: &mut ScanReport) {
    let grade = rules::rules()
        .iter()
        .find(|item| item.code == rule)
        .map(|item| item.grade)
        .unwrap_or(Grade::Exec);
    push_hit(root, path, 0, rule, grade, vec![rule], "", report);
}

fn push_hit(
    root: &Path,
    path: &Path,
    line: usize,
    rule: &'static str,
    grade: Grade,
    matched_rules: Vec<&'static str>,
    snippet: &str,
    report: &mut ScanReport,
) {
    let snippet: String = snippet.chars().take(80).collect();
    let snippet_hash = hex::encode(Sha256::digest(snippet.as_bytes()));
    let description = rules::rules()
        .iter()
        .find(|item| item.code == rule)
        .map(|item| item.description)
        .unwrap_or("");
    report.hits.push(Hit {
        file: relative(root, path),
        line,
        rule,
        grade: grade.as_str(),
        description,
        matched_rules,
        snippet_hash,
        waived_by: None,
    });
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nq-interpreter-refs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_excludes_agent_dirs_and_grades_files() {
        let dir = workspace("scan");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/hidden.py"), "python -m nanoquant").unwrap();
        std::fs::write(dir.join("run.py"), "print('x')").unwrap();
        std::fs::write(dir.join("requirements-dev.txt"), "pytest").unwrap();
        std::fs::write(dir.join("README.md"), "Python 3.11\npython -m nanoquant\n").unwrap();
        let report = scan(&ScanOptions::new(&dir));
        assert!(report.excluded.iter().any(|path| path == ".git"));
        assert!(report
            .hits
            .iter()
            .any(|hit| hit.file == "run.py" && hit.rule == "exec.python_script"));
        assert!(report
            .hits
            .iter()
            .any(|hit| hit.rule == "exec.requirements"));
        assert!(report
            .hits
            .iter()
            .any(|hit| hit.file == "README.md" && hit.grade == "exec"));
        let readme_line = report
            .hits
            .iter()
            .find(|hit| hit.file == "README.md" && hit.line == 1)
            .unwrap();
        assert_eq!(readme_line.grade, "text");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_line_gets_highest_grade_and_keeps_matched_rules() {
        let dir = workspace("priority");
        std::fs::write(dir.join("note.md"), "python -m nanoquant\n").unwrap();
        let report = scan(&ScanOptions::new(&dir));
        let hit = report.hits.first().unwrap();
        assert_eq!(hit.grade, "exec");
        assert!(hit.matched_rules.contains(&"exec.python_module"));
        assert!(hit.matched_rules.contains(&"text.python_word"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn max_depth_marks_truncated() {
        let dir = workspace("depth");
        let nested = dir.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("run.py"), "print('x')").unwrap();
        let mut options = ScanOptions::new(&dir);
        options.max_depth = 1;
        let report = scan(&options);
        assert!(report.truncated);
        assert!(report.hits.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn large_file_is_skipped_with_size() {
        let dir = workspace("large");
        let content = "x".repeat((MAX_TEXT_BYTES + 10) as usize);
        std::fs::write(dir.join("README.md"), content).unwrap();
        let report = scan(&ScanOptions::new(&dir));
        assert_eq!(report.skipped_large.len(), 1);
        assert!(report.skipped_large[0].bytes > MAX_TEXT_BYTES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hits_are_sorted_deterministically() {
        let dir = workspace("sort");
        std::fs::write(dir.join("b.md"), "python 3.\n").unwrap();
        std::fs::write(dir.join("a.md"), "python 3.\n").unwrap();
        let report = scan(&ScanOptions::new(&dir));
        let files: Vec<_> = report.hits.iter().map(|hit| hit.file.as_str()).collect();
        assert_eq!(files, vec!["a.md", "b.md"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_file_list_scans_only_listed_files_without_recursion() {
        let dir = workspace("changed");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("a.md"), "python 3.\n").unwrap();
        std::fs::write(dir.join("b.md"), "python 3.\n").unwrap();
        std::fs::write(dir.join("nested/c.md"), "python 3.\n").unwrap();
        let list = dir.join("changed.txt");
        std::fs::write(&list, "a.md\nnested/c.md\nnested\n").unwrap();
        let report = scan_changed_files(&ScanOptions::new(&dir), &list);
        let files: Vec<_> = report.hits.iter().map(|hit| hit.file.as_str()).collect();
        assert_eq!(files, vec!["a.md", "nested/c.md"]);
        assert!(report.skipped_missing.iter().any(|path| path == "nested"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_file_list_rejects_absolute_and_parent_paths() {
        let dir = workspace("changed-unsafe");
        std::fs::write(dir.join("a.md"), "python 3.\n").unwrap();
        let list = dir.join("changed.txt");
        let absolute = dir.join("a.md").to_string_lossy().replace('\\', "/");
        std::fs::write(
            &list,
            format!("{absolute}\n../escape.md\nnested/../../x.md\n"),
        )
        .unwrap();
        let report = scan_changed_files(&ScanOptions::new(&dir), &list);
        assert!(report.hits.is_empty());
        assert_eq!(report.rejected.len(), 3);
        assert!(report.rejected.iter().any(|path| path == &absolute));
        assert!(report.rejected.iter().any(|path| path == "../escape.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_and_non_file_entries_are_registered_not_scanned() {
        let dir = workspace("changed-missing");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        let list = dir.join("changed.txt");
        std::fs::write(&list, "gone.md\nnested\n").unwrap();
        let report = scan_changed_files(&ScanOptions::new(&dir), &list);
        assert!(report.hits.is_empty());
        assert!(report.skipped_missing.iter().any(|path| path == "gone.md"));
        assert!(report.skipped_missing.iter().any(|path| path == "nested"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn graded_counts_only_four_grades() {
        let mut report = ScanReport::default();
        finalize(&mut report);
        let keys = report.graded.keys().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(keys, vec!["artifact", "exec", "naming", "text"]);
    }

    #[test]
    fn out_path_inside_scan_root_is_not_allowed() {
        let dir = workspace("out-path");
        assert!(!out_path_is_allowed(&dir, &dir.join("report.json")));
        assert!(!out_path_is_allowed(&dir, &dir.join("docs/../report.json")));
        assert!(!out_path_is_allowed(&dir, Path::new("report.json")));
        assert!(out_path_is_allowed(
            &dir,
            &dir.join(".codeartsdoer/evidence/report.json")
        ));
        assert!(out_path_is_allowed(
            &dir,
            &std::env::temp_dir().join("nq-out-outside.json")
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_file_is_registered() {
        let dir = workspace("unreadable");
        let mut report = ScanReport::default();
        let options = ScanOptions::new(&dir);
        scan_text(&options, &dir, &dir.join("missing.md"), &mut report);
        assert_eq!(report.unreadable, vec!["missing.md"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snippet_is_hashed_and_not_embedded() {
        let dir = workspace("snippet");
        std::fs::write(dir.join("README.md"), "python -m secret_token\n").unwrap();
        let report = scan(&ScanOptions::new(&dir));
        let hit = report.hits.first().unwrap();
        assert_eq!(hit.snippet_hash.len(), 64);
        assert!(!serde_json::to_string(hit).unwrap().contains("secret_token"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
