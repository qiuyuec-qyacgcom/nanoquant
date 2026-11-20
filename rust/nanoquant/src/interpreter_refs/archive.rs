//! 历史归档标记、命令前缀与文档分层校验。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::rules;

const MARKER: &str = "> 历史记录：已退役，不可执行";
const SOURCE_PREFIX: &str = "> 源路径：";
const TIME_PREFIX: &str = "> 归档时间：";
const RESTRICTED_TOP: &[&str] = &["deploy", "scripts", ".github"];
const CURRENT_DELIVERY: &[&str] = &[
    "README.md",
    "docs/RUNBOOK.md",
    "docs/MAINNET_DRILL.md",
    "docs/SIGNAL_EVENTS.md",
    "docs/EXEC_PROTOCOL.md",
    "docs/STRATEGY.md",
    "deploy/README.md",
    "docs/archive/history/INDEX.md",
];

#[derive(Debug, Default)]
pub(crate) struct ArchiveReport {
    pub files: usize,
    pub missing_marker: Vec<String>,
    pub marker_position: Vec<String>,
    pub command_block: Vec<String>,
    pub misplaced: Vec<String>,
    pub layer_conflict: Vec<String>,
    pub exec_in_archive: usize,
}

impl ArchiveReport {
    pub(crate) fn is_valid(&self) -> bool {
        self.missing_marker.is_empty()
            && self.marker_position.is_empty()
            && self.command_block.is_empty()
            && self.misplaced.is_empty()
            && self.layer_conflict.is_empty()
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "files": self.files,
            "missing_marker": self.missing_marker,
            "marker_position": self.marker_position,
            "command_block": self.command_block,
            "misplaced": self.misplaced,
            "layer_conflict": self.layer_conflict,
            "exec_in_archive": self.exec_in_archive,
        })
    }
}

pub(crate) fn validate(root: &Path, archive_root: &Path) -> ArchiveReport {
    let mut report = ArchiveReport::default();
    let Ok(relative_archive) = archive_root.strip_prefix(root) else {
        report.misplaced.push(display(archive_root));
        return report;
    };
    let relative_archive = normalize(relative_archive);
    let top = relative_archive.split('/').next().unwrap_or_default();
    if relative_archive.is_empty() || RESTRICTED_TOP.contains(&top) {
        report.misplaced.push(relative_archive);
        return report;
    }
    if !archive_root.is_dir() {
        report.misplaced.push(relative_archive);
        return report;
    }
    let mut files = Vec::new();
    collect(archive_root, &mut files);
    files.retain(|path| {
        !(path.parent() == Some(archive_root)
            && path.file_name().and_then(|name| name.to_str()) == Some("INDEX.md"))
    });
    files.sort();
    report.files = files.len();
    for path in &files {
        let relative = relative_to(archive_root, path);
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        check_marker(&text, &relative, &mut report);
        check_commands(&text, &relative, &mut report);
    }
    check_layers(root, archive_root, &files, &mut report);
    report
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().and_then(|item| item.to_str()) == Some("md") {
            files.push(path);
        }
    }
}

fn check_marker(text: &str, relative: &str, report: &mut ArchiveReport) {
    let head: Vec<&str> = text.lines().take(5).collect();
    let head_has = |prefix: &str| {
        head.iter()
            .any(|line| line.trim_start().starts_with(prefix))
    };
    if head_has(MARKER) && head_has(SOURCE_PREFIX) && head_has(TIME_PREFIX) {
        return;
    }
    if text
        .lines()
        .any(|line| line.trim_start().starts_with(MARKER))
    {
        report.marker_position.push(relative.to_string());
    } else {
        report.missing_marker.push(relative.to_string());
    }
}

fn check_commands(text: &str, relative: &str, report: &mut ArchiveReport) {
    let mut fence: Option<String> = None;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if is_fence {
            if fence.is_some() {
                fence = None;
            } else {
                fence = Some(trimmed.chars().take(3).collect());
            }
            continue;
        }
        if fence.is_none() || !is_interpreter_command(line) {
            continue;
        }
        if !trimmed.starts_with("# [已删除]") {
            report.exec_in_archive += 1;
            report
                .command_block
                .push(format!("{relative}:{}", index + 1));
        }
    }
}

fn is_interpreter_command(line: &str) -> bool {
    rules::rules()
        .iter()
        .filter(|rule| {
            matches!(
                rule.code,
                "exec.python_module"
                    | "exec.python_inline"
                    | "exec.pip_install"
                    | "exec.command_line"
                    | "exec.venv_path"
                    | "exec.shebang"
            )
        })
        .any(|rule| rules::matches_line(rule, line))
}

fn check_layers(root: &Path, archive_root: &Path, files: &[PathBuf], report: &mut ArchiveReport) {
    for current in CURRENT_DELIVERY {
        let path = root.join(current);
        if !path.is_file() || path.starts_with(archive_root) {
            continue;
        }
        for archived in files {
            if relative_to(archive_root, archived) == *current {
                report.layer_conflict.push(format!(
                    "{current} <-> {}",
                    relative_to(archive_root, archived)
                ));
            }
        }
    }
}

fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(normalize)
        .unwrap_or_else(|_| display(path))
}

fn normalize(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    const MARKER: &str = "> 历史记录：已退役，不可执行";

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nq-interpreter-refs-archive-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("docs/archive/history")).unwrap();
        dir
    }

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn valid_doc(body: &str) -> String {
        format!("{MARKER}\n> 源路径：docs/example.md\n> 归档时间：2026-11-20T00:00:00Z\n\n{body}\n")
    }

    #[test]
    fn valid_archive_passes_without_findings() {
        let dir = workspace("valid");
        write(&dir, "docs/archive/history/docs/a.md", &valid_doc("# A"));
        let report = validate(&dir, &dir.join("docs/archive/history"));
        assert!(
            report.missing_marker.is_empty(),
            "{:?}",
            report.missing_marker
        );
        assert!(report.marker_position.is_empty());
        assert!(report.command_block.is_empty());
        assert!(report.misplaced.is_empty());
        assert_eq!(report.files, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_marker_is_reported() {
        let dir = workspace("missing");
        write(&dir, "docs/archive/history/docs/a.md", "# A\n");
        let report = validate(&dir, &dir.join("docs/archive/history"));
        assert_eq!(report.missing_marker, vec!["docs/a.md"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_after_five_lines_is_position_violation() {
        let dir = workspace("position");
        let content = "# A\n\nline\n\nline\n\nline\n\n".to_string() + &valid_doc("body");
        write(&dir, "docs/archive/history/docs/a.md", &content);
        let report = validate(&dir, &dir.join("docs/archive/history"));
        assert_eq!(report.marker_position, vec!["docs/a.md"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unmarked_command_in_fence_counts_as_exec() {
        let dir = workspace("exec");
        let body = "~~~text\npython -m nanoquant\n~~~";
        write(&dir, "docs/archive/history/docs/a.md", &valid_doc(body));
        let report = validate(&dir, &dir.join("docs/archive/history"));
        assert_eq!(report.exec_in_archive, 1);
        assert_eq!(report.command_block, vec!["docs/a.md:6"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleted_prefix_is_accepted() {
        let dir = workspace("deleted");
        let body = "~~~text\n# [已删除] python -m nanoquant\n~~~";
        write(&dir, "docs/archive/history/docs/a.md", &valid_doc(body));
        let report = validate(&dir, &dir.join("docs/archive/history"));
        assert_eq!(report.exec_in_archive, 0);
        assert!(report.command_block.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_root_in_deploy_is_misplaced() {
        let dir = workspace("deploy");
        std::fs::create_dir_all(dir.join("deploy/history")).unwrap();
        write(&dir, "deploy/history/a.md", &valid_doc("# A"));
        let report = validate(&dir, &dir.join("deploy/history"));
        assert!(!report.misplaced.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn repo_root() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn same_name_in_delivery_and_archive_is_layer_conflict() {
        let dir = workspace("conflict");
        write(&dir, "docs/RUNBOOK.md", "# 当前运行手册\n");
        write(
            &dir,
            "docs/archive/history/docs/RUNBOOK.md",
            &valid_doc("# 旧手册"),
        );
        let report = validate(&dir, &dir.join("docs/archive/history"));
        assert!(!report.layer_conflict.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn committed_archive_passes() {
        let root = repo_root();
        let report = validate(&root, &root.join("docs/archive/history"));
        assert!(report.files >= 9, "files={}", report.files);
        assert!(
            report.missing_marker.is_empty(),
            "{:?}",
            report.missing_marker
        );
        assert!(
            report.marker_position.is_empty(),
            "{:?}",
            report.marker_position
        );
        assert!(
            report.command_block.is_empty(),
            "{:?}",
            report.command_block
        );
        assert!(report.misplaced.is_empty(), "{:?}", report.misplaced);
        assert!(
            report.layer_conflict.is_empty(),
            "{:?}",
            report.layer_conflict
        );
    }
}
