//! 推理模型库。对齐 `ai/reasoning.py`。
//!
//! 思考强度只对识别出的推理模型注入，普通模型传了会被网关 400。
//! 排除名单优先于命中名单：`qwen2.5` 含 `qwen-2` 的近亲写法，不能被 `qwen3` 误伤，
//! 但 `gpt-4o` 这种明确排除项必须先挡掉。

use axum::extract::Query;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

const LEVELS: [&str; 8] = [
    "auto", "none", "minimal", "low", "medium", "high", "xhigh", "max",
];
const EXTREME: [&str; 2] = ["xhigh", "max"];

struct Family {
    key: &'static str,
    label: &'static str,
    hints: &'static [&'static str],
    extreme: bool,
    note: &'static str,
}

const FAMILIES: &[Family] = &[
    Family {
        key: "openai-gpt6",
        label: "OpenAI GPT-6 / ChatGPT-6",
        hints: &["gpt-6", "chatgpt-6"],
        extreme: true,
        note: "",
    },
    Family {
        key: "openai-gpt5",
        label: "OpenAI GPT-5",
        hints: &["gpt-5"],
        extreme: true,
        note: "",
    },
    Family {
        key: "openai-o-series",
        label: "OpenAI o1 / o3 / o4",
        hints: &["o1", "o3", "o4"],
        extreme: true,
        note: "",
    },
    Family {
        key: "gpt-oss",
        label: "GPT-OSS 开源推理",
        hints: &["gpt-oss"],
        extreme: false,
        note: "vLLM/Ollama 常见",
    },
    Family {
        key: "anthropic-claude",
        label: "Anthropic Claude 3.7+ / 4",
        hints: &[
            "claude-3-7",
            "claude-3.7",
            "claude-4",
            "claude-opus-4",
            "claude-sonnet-4",
        ],
        extreme: false,
        note: "Anthropic 协议走 thinking 预算",
    },
    Family {
        key: "google-gemini",
        label: "Google Gemini 2.5 / 3",
        hints: &["gemini-2.5", "gemini-2-5", "gemini-3"],
        extreme: false,
        note: "",
    },
    Family {
        key: "deepseek-v4",
        label: "DeepSeek V4 / V4.1",
        hints: &["deepseek-v4", "v4.1"],
        extreme: false,
        note: "",
    },
    Family {
        key: "deepseek-r1",
        label: "DeepSeek R1 / Reasoner",
        hints: &["deepseek-r1", "deepseek-reasoner", "-r1"],
        extreme: false,
        note: "",
    },
    Family {
        key: "qwen3",
        label: "Qwen3 全系 / QwQ",
        hints: &["qwen3", "qwen-3", "qwq"],
        extreme: false,
        note: "含 qwen3-vl",
    },
    Family {
        key: "kimi",
        label: "Kimi K2.7 / K3",
        hints: &["kimi-k2.7", "kimi-k3"],
        extreme: false,
        note: "",
    },
    Family {
        key: "glm",
        label: "GLM-5",
        hints: &["glm-5"],
        extreme: false,
        note: "",
    },
    Family {
        key: "minimax",
        label: "MiniMax M2",
        hints: &["minimax-m2"],
        extreme: false,
        note: "",
    },
    Family {
        key: "mimo",
        label: "MiMo V2",
        hints: &["mimo-v2"],
        extreme: false,
        note: "",
    },
    Family {
        key: "grok",
        label: "xAI Grok 3 mini / 4",
        hints: &["grok-3-mini", "grok-4"],
        extreme: true,
        note: "",
    },
];

const NON_REASONING: [&str; 9] = [
    "gpt-4o", "gpt-4.1", "gpt-3", "qwen2.5", "qwen-2.5", "llama", "mistral", "-chat", "instruct",
];

const LABELS: [(&str, &str); 8] = [
    ("auto", "自动 (auto)"),
    ("none", "关闭 (none)"),
    ("minimal", "极简 (minimal)"),
    ("low", "低 (low)"),
    ("medium", "中 (medium)"),
    ("high", "高 (high)"),
    ("xhigh", "超高 (xhigh)"),
    ("max", "极值 (max)"),
];

pub fn routes() -> Router {
    Router::new().route("/ai/reasoning-models", get(reasoning_models))
}

#[derive(Deserialize)]
struct ModelQuery {
    #[serde(default)]
    model: String,
}

async fn reasoning_models(Query(q): Query<ModelQuery>) -> Json<Value> {
    let mut data = json!({
        "levels": LEVELS,
        "extreme_levels": EXTREME,
        "families": FAMILIES.iter().map(|f| json!({"key": f.key, "label": f.label, "hints": f.hints, "extreme": f.extreme, "note": f.note})).collect::<Vec<_>>(),
        "non_reasoning_hints": NON_REASONING,
    });
    if !q.model.is_empty() {
        data["matched"] = match match_family(&q.model) {
            Some(f) => {
                json!({"key": f.key, "label": f.label, "extreme": f.extreme, "note": f.note})
            }
            None => Value::Null,
        };
        data["options"] = json!(effort_options(&q.model));
    }
    data.into()
}

fn match_family(model: &str) -> Option<&'static Family> {
    let low = model.to_lowercase();
    if low.is_empty() || NON_REASONING.iter().any(|hint| low.contains(hint)) {
        return None;
    }
    FAMILIES
        .iter()
        .find(|f| f.hints.iter().any(|hint| low.contains(hint)))
}

/// 思考强度归一化：非法值一律视为 `auto`，不向服务端发送脏参数。
pub(crate) fn normalize_effort(value: &str) -> &'static str {
    let text = value.trim().to_ascii_lowercase();
    LEVELS
        .iter()
        .find(|level| **level == text)
        .copied()
        .unwrap_or("auto")
}

/// 模型是否支持 `reasoning_effort`。普通模型（`gpt-4o` 等）传了会被网关 400。
pub(crate) fn supports_effort(model: &str) -> bool {
    match_family(model).is_some()
}

fn anthropic_budget(level: &str) -> i64 {
    match level {
        "minimal" => 1024,
        "low" => 2048,
        "medium" => 8000,
        "high" => 16000,
        "xhigh" => 32000,
        "max" => 64000,
        _ => 8000,
    }
}

/// 按协议把思考强度写进请求体，返回注入的键（供 400/422 时整体回退）。
///
/// - `auto` 不注入，交给服务端默认；
/// - Anthropic 走 `thinking.budget_tokens`，并要求 `max_tokens > budget`；
/// - OpenAI Responses 走 `reasoning.effort`；
/// - OpenAI Chat 只对识别出的推理模型注入 `reasoning_effort`。
pub(crate) fn apply_effort(
    payload: &mut Value,
    protocol: &str,
    model: &str,
    effort: &str,
    base_max_tokens: i64,
) -> Vec<String> {
    let level = normalize_effort(effort);
    if level == "auto" {
        return Vec::new();
    }
    if protocol == "anthropic_messages" {
        if level == "none" {
            payload["thinking"] = json!({"type": "disabled"});
            return vec!["thinking".to_string()];
        }
        let mut budget = anthropic_budget(level);
        let wanted = budget + base_max_tokens.max(256);
        let max_tokens = wanted.min(64000);
        if max_tokens <= budget {
            budget = (max_tokens - 256).max(1024);
        }
        payload["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        payload["max_tokens"] = json!(max_tokens);
        return vec!["thinking".to_string(), "max_tokens".to_string()];
    }
    if protocol == "openai_responses" {
        let mapped = match level {
            "xhigh" | "max" => "high",
            "minimal" => "low",
            other => other,
        };
        payload["reasoning"] = json!({"effort": mapped});
        return vec!["reasoning".to_string()];
    }
    if !supports_effort(model) {
        return Vec::new();
    }
    payload["reasoning_effort"] = json!(level);
    vec!["reasoning_effort".to_string()]
}

fn effort_options(model: &str) -> Vec<Value> {
    let extreme = match_family(model).map(|f| f.extreme).unwrap_or(false);
    LEVELS
        .iter()
        .map(|value| {
            let requires = EXTREME.contains(value);
            json!({"value": value, "label": LABELS.iter().find(|(k, _)| k == value).unwrap().1, "requires_extreme": requires, "enabled": !requires || extreme})
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn excluded_models_lose_to_reasoning_families() {
        let app = Router::new().nest("/api/v1", routes());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ai/reasoning-models?model=gpt-4o-mini")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            body["matched"].is_null(),
            "gpt-4o 在排除名单里，不能因含 o 被当成 o 系列"
        );
        let max = body["options"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["value"] == "max")
            .unwrap();
        assert_eq!(max["enabled"], false, "非推理模型的极值档应置灰而不是隐藏");
    }

    #[tokio::test]
    async fn first_matching_family_wins_and_unlocks_extreme() {
        let app = Router::new().nest("/api/v1", routes());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ai/reasoning-models?model=Grok-4-fast")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["matched"]["key"], "grok", "匹配应忽略大小写");
        assert_eq!(body["options"].as_array().unwrap().len(), 8);
        assert_eq!(body["options"][7]["enabled"], true, "grok 支持极值档");
        assert_eq!(body["families"].as_array().unwrap().len(), FAMILIES.len());
    }

    #[test]
    fn non_reasoning_models_never_receive_reasoning_effort() {
        for model in ["gpt-4o", "gpt-4o-mini", "gpt-4.1", "qwen2.5-72b", "llama-3"] {
            assert!(!supports_effort(model), "{model} 不应被识别为推理模型");
            let mut payload = json!({"model": model});
            let applied = apply_effort(&mut payload, "openai_chat", model, "high", 4096);
            assert!(applied.is_empty(), "{model} 注入键应为空");
            assert!(
                payload.get("reasoning_effort").is_none(),
                "{model} 不能收到 reasoning_effort"
            );
        }
    }

    #[test]
    fn anthropic_effort_maps_to_thinking_budget_and_keeps_max_tokens_above_it() {
        let mut payload = json!({"model": "claude-4-sonnet", "max_tokens": 4096});
        let applied = apply_effort(
            &mut payload,
            "anthropic_messages",
            "claude-4-sonnet",
            "high",
            4096,
        );
        assert_eq!(
            applied,
            vec!["thinking".to_string(), "max_tokens".to_string()]
        );
        assert_eq!(payload["thinking"]["budget_tokens"], 16000);
        assert_eq!(payload["max_tokens"], 20096, "max_tokens 必须大于思考预算");
    }

    #[test]
    fn anthropic_none_disables_thinking_and_openai_responses_clamps_extreme() {
        let mut payload = json!({"model": "claude-4"});
        apply_effort(&mut payload, "anthropic_messages", "claude-4", "none", 4096);
        assert_eq!(payload["thinking"]["type"], "disabled");

        let mut payload = json!({"model": "gpt-5"});
        let applied = apply_effort(&mut payload, "openai_responses", "gpt-5", "max", 4096);
        assert_eq!(applied, vec!["reasoning".to_string()]);
        assert_eq!(payload["reasoning"]["effort"], "high", "极值档映射到 high");

        let mut payload = json!({"model": "gpt-5"});
        let applied = apply_effort(&mut payload, "openai_responses", "gpt-5", "auto", 4096);
        assert!(applied.is_empty());
    }

    #[test]
    fn effort_normalization_defaults_to_auto() {
        assert_eq!(normalize_effort("HIGH"), "high");
        assert_eq!(normalize_effort("  xhigh "), "xhigh");
        assert_eq!(normalize_effort("turbo"), "auto");
        assert_eq!(normalize_effort(""), "auto");
    }
}
