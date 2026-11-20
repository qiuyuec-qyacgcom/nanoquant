use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::rules;
use super::scanner::ScanReport;

pub(crate) fn build(
    scope: &str,
    hits: &Value,
    scan: &ScanReport,
    process: &[String],
    image: &[Value],
    image_error: &str,
    exceptions: &Value,
    archive: &Value,
    checks: &Value,
    exit_code: i32,
) -> Value {
    let report_sha256 = hits_sha256(hits);
    let unregistered_text = scan
        .hits
        .iter()
        .filter(|hit| hit.grade == "text" && hit.waived_by.is_none())
        .count();
    json!({
        "commit": std::env::var("GIT_COMMIT").unwrap_or_else(|_| "unknown".to_string()),
        "generated_at": crate::now(),
        "rules_version": rules::rules_version(),
        "scope": scope,
        "hits": hits,
        "graded": scan.graded,
        "unregistered_text": unregistered_text,
        "unreadable": scan.unreadable,
        "excluded": scan.excluded,
        "skipped_large": scan.skipped_large,
        "truncated": scan.truncated,
        "exceptions": exceptions,
        "archive": archive,
        "checks": checks,
        "process": process,
        "image": image,
        "image_error": image_error,
        "report_sha256": report_sha256,
        "exit_code": exit_code,
        "result": if exit_code == 0 { "pass" } else { "hit" },
    })
}

pub(crate) fn hits_sha256(hits: &Value) -> String {
    let canonical = serde_json::to_string(hits).unwrap_or_else(|_| "[]".to_string());
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter_refs::scanner::{Hit, ScanReport};
    use std::collections::BTreeMap;

    #[test]
    fn report_contains_hashes_and_no_plaintext_snippet() {
        let hit = Hit {
            file: "README.md".to_string(),
            line: 1,
            rule: "text.python_word",
            grade: "text",
            matched_rules: vec!["text.python_word"],
            snippet_hash: "a".repeat(64),
            waived_by: None,
        };
        let mut graded = BTreeMap::new();
        graded.insert("text".to_string(), 1);
        let scan = ScanReport {
            hits: vec![hit],
            graded,
            ..ScanReport::default()
        };
        let hits = serde_json::to_value(&scan.hits).unwrap();
        let report = build(
            "text",
            &hits,
            &scan,
            &[],
            &[],
            "",
            &json!([]),
            &json!([]),
            &json!([]),
            5,
        );
        assert_eq!(report["rules_version"], "3.0.0");
        assert_eq!(report["exit_code"], 5);
        assert_eq!(report["unregistered_text"], 1);
        assert_eq!(report["report_sha256"].as_str().unwrap().len(), 64);
        assert!(!report.to_string().contains("python -m"));
    }

    #[test]
    fn report_sha256_depends_on_final_hits() {
        let empty = json!([]);
        let hit = json!([{
            "file": "a.md",
            "line": 1,
            "rule": "text.python_word",
            "grade": "text",
            "matched_rules": ["text.python_word"],
            "snippet_hash": "x",
            "waived_by": null
        }]);
        assert_ne!(hits_sha256(&empty), hits_sha256(&hit));
        assert_eq!(hits_sha256(&hit), hits_sha256(&hit.clone()));
    }
}
