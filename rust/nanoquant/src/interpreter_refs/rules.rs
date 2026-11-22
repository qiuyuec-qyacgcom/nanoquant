pub(crate) const RULES_VERSION: &str = "3.0.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Grade {
    Exec,
    Naming,
    Artifact,
    Text,
    Docs,
}

impl Grade {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Grade::Exec => "exec",
            Grade::Text => "text",
            Grade::Naming => "naming",
            Grade::Artifact => "artifact",
            Grade::Docs => "docs",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Rule {
    pub code: &'static str,
    pub grade: Grade,
    /// 规则说明；当前只按 code 分派，尚未读取说明文本，接线后取消豁免。
    #[allow(dead_code)]
    pub description: &'static str,
}

pub(crate) const RULES: &[Rule] = &[
    Rule {
        code: "exec.python_module",
        grade: Grade::Exec,
        description: "解释器模块启动命令",
    },
    Rule {
        code: "exec.python_inline",
        grade: Grade::Exec,
        description: "解释器内联执行命令",
    },
    Rule {
        code: "exec.python_image",
        grade: Grade::Exec,
        description: "解释器基础镜像标签",
    },
    Rule {
        code: "exec.pip_install",
        grade: Grade::Exec,
        description: "解释器依赖安装命令",
    },
    Rule {
        code: "exec.import_nanoquant",
        grade: Grade::Exec,
        description: "解释器包导入",
    },
    Rule {
        code: "exec.rollback_switch",
        grade: Grade::Exec,
        description: "迁移期内核回滚开关",
    },
    Rule {
        code: "exec.python_script",
        grade: Grade::Exec,
        description: "解释器脚本文件",
    },
    Rule {
        code: "exec.python_env",
        grade: Grade::Exec,
        description: "虚拟环境或字节码目录",
    },
    Rule {
        code: "exec.requirements",
        grade: Grade::Exec,
        description: "解释器依赖清单",
    },
    Rule {
        code: "exec.venv_path",
        grade: Grade::Exec,
        description: "虚拟环境解释器路径",
    },
    Rule {
        code: "exec.shebang",
        grade: Grade::Exec,
        description: "脚本解释器声明",
    },
    Rule {
        code: "exec.command_line",
        grade: Grade::Exec,
        description: "命令行解释器调用",
    },
    Rule {
        code: "exec.dep_tool",
        grade: Grade::Exec,
        description: "解释器工具配置",
    },
    Rule {
        code: "exec.archive_command",
        grade: Grade::Exec,
        description: "归档区可执行命令未标记删除",
    },
    Rule {
        code: "text.python_word",
        grade: Grade::Text,
        description: "文本中出现解释器词汇",
    },
    Rule {
        code: "text.python_badge",
        grade: Grade::Text,
        description: "技术栈徽章仍宣称解释器",
    },
    Rule {
        code: "text.python_version",
        grade: Grade::Text,
        description: "文档仍声明解释器版本",
    },
    Rule {
        code: "text.python_dependency",
        grade: Grade::Text,
        description: "文档仍描述解释器依赖",
    },
    Rule {
        code: "naming.rust_test",
        grade: Grade::Naming,
        description: "Rust 测试函数名含解释器词汇",
    },
    Rule {
        code: "naming.rust_ident",
        grade: Grade::Naming,
        description: "Rust 标识符含解释器词汇",
    },
    Rule {
        code: "naming.frontend_field",
        grade: Grade::Naming,
        description: "前端字段含解释器词汇",
    },
    Rule {
        code: "naming.config_key",
        grade: Grade::Naming,
        description: "配置键含解释器词汇",
    },
    Rule {
        code: "artifact.report_json",
        grade: Grade::Artifact,
        description: "过期扫描报告生成物",
    },
    Rule {
        code: "artifact.target_dump",
        grade: Grade::Artifact,
        description: "过期编译或测试输出",
    },
    Rule {
        code: "artifact.backup_copy",
        grade: Grade::Artifact,
        description: "改名保留的可执行副本",
    },
    Rule {
        code: "docs.missing_marker",
        grade: Grade::Docs,
        description: "历史归档缺少退役标记",
    },
    Rule {
        code: "docs.marker_position",
        grade: Grade::Docs,
        description: "退役标记不在首部",
    },
    Rule {
        code: "docs.command_block",
        grade: Grade::Docs,
        description: "归档命令块未标记删除",
    },
    Rule {
        code: "docs.misplaced_archive",
        grade: Grade::Docs,
        description: "归档位于执行面",
    },
    Rule {
        code: "docs.layer_conflict",
        grade: Grade::Docs,
        description: "文档分层冲突",
    },
];

pub(crate) fn rules_version() -> &'static str {
    RULES_VERSION
}

pub(crate) fn rules() -> &'static [Rule] {
    RULES
}

pub(crate) fn coarse(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.contains("python") || lower.contains("pip")
}

pub(crate) fn matches_line(rule: &Rule, line: &str) -> bool {
    let lower = line.to_lowercase();
    let trimmed = lower.trim_start();
    match rule.code {
        "exec.python_module" => lower.contains("python -m") || lower.contains("python3 -m"),
        "exec.python_inline" => lower.contains("python -c") || lower.contains("python3 -c"),
        "exec.python_image" => lower.contains("python:") || lower.contains("python3:"),
        "exec.pip_install" => lower.contains("pip install") || lower.contains("pip3 install"),
        "exec.import_nanoquant" => lower.contains("import nanoquant"),
        "exec.rollback_switch" => lower.contains("nanoquant_rollback_kernel"),
        "exec.venv_path" => {
            lower.contains(".venv/bin/python") || lower.contains(".venv\\scripts\\python")
        }
        "exec.shebang" => trimmed.starts_with("#!") && lower.contains("python"),
        "exec.command_line" => {
            let command = trimmed.trim_start_matches(['$', '#', '>', ' ']);
            command.starts_with("py -3")
                || ["python ", "python3 "].iter().any(|prefix| {
                    command.strip_prefix(prefix).is_some_and(|rest| {
                        rest.starts_with('-')
                            || rest.starts_with("./")
                            || rest.ends_with(".py")
                            || (rest.contains('/') && !rest.starts_with('/'))
                    })
                })
        }
        "text.python_word" => lower.contains("python"),
        "text.python_badge" => lower.contains("img.shields.io/badge/python"),
        "text.python_version" => lower.contains("python 3.") || lower.contains("python3."),
        "text.python_dependency" => {
            lower.contains("requirements.txt")
                || lower.contains("pyproject.toml")
                || lower.contains("pip ")
        }
        "naming.rust_test" => {
            (trimmed.starts_with("fn ") || trimmed.starts_with("async fn "))
                && lower.contains("python")
        }
        "naming.rust_ident" => {
            lower.contains("_python_") || lower.contains("python_") || lower.contains("_python")
        }
        "naming.frontend_field" => {
            lower.contains("?.python") || lower.contains(".python") || lower.contains("python:")
        }
        "naming.config_key" => {
            lower.contains("\"python") || lower.contains("python=") || lower.contains("python:")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn markdown_title_is_not_a_command_line() {
        let command_line = rules()
            .iter()
            .find(|rule| rule.code == "exec.command_line")
            .expect("exec.command_line 必须存在");
        assert!(
            !matches_line(command_line, "# Python / Rust API 路由差异清单"),
            "Markdown 标题不得被判为命令行"
        );
        assert!(matches_line(command_line, "python scripts/run.py"));
        assert!(matches_line(command_line, "python -m nanoquant"));
    }

    #[test]
    fn rule_set_has_expected_grades_and_unique_ids() {
        assert_eq!(rules_version(), "3.0.0");
        let counts = [
            (Grade::Exec, 14),
            (Grade::Text, 4),
            (Grade::Naming, 4),
            (Grade::Artifact, 3),
            (Grade::Docs, 5),
        ];
        for (grade, expected) in counts {
            assert_eq!(
                rules().iter().filter(|rule| rule.grade == grade).count(),
                expected,
                "{grade:?}"
            );
        }
        let mut seen = HashSet::new();
        for rule in rules() {
            assert!(seen.insert(rule.code), "重复规则 {}", rule.code);
            assert!(!rule.description.trim().is_empty(), "{} 缺说明", rule.code);
        }
    }
}
