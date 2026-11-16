//! P3-3：保护单（STOP_MARKET + TAKE_PROFIT_MARKET）并行构造与签名。
//!
//! 阶段目标：把「开仓后立刻挂止损/止盈」这两笔请求的**构造 + 签名**前移到 Rust，
//! 用 `tokio::join!` 真正并行准备。本阶段**不代发**：只回传有序参数对与签名，
//! 由 Python 侧逐字节比对，一致后再考虑让 sidecar 直接发单。
//!
//! 为什么先只构造不发：
//! - 保护单是 `closePosition=true` 的条件单，一旦参数错（价格精度、positionSide、
//!   workingType）就是把仓位裸奔或误平，风险远高于普通下单；
//! - 构造 + 签名的正确性可以用「与 Python 逐字节一致」严格证明，而发单还牵涉
//!   网关限频、-4120 回落、结果未知等语义，留到下一阶段。
//!
//! 编码一致性（关键）：Python 用 `urllib.parse.urlencode(payload, doseq=True)`，
//! 即 `quote_plus` 且保留 `A-Za-z0-9_.-~`、空格转 `+`、其余 `%XX` 大写。
//! 本模块 `form_urlencode` 必须逐字节复刻这套规则，否则比对会假失败。

use serde::Serialize;

use crate::sign::Signer;

/// 保护单类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectKind {
    StopMarket,
    TakeProfitMarket,
}

impl ProtectKind {
    pub fn as_api_type(&self) -> &'static str {
        match self {
            Self::StopMarket => "STOP_MARKET",
            Self::TakeProfitMarket => "TAKE_PROFIT_MARKET",
        }
    }
}

/// 一笔待签名的保护单请求。
#[derive(Debug, Clone, Serialize)]
pub struct ProtectionRequest {
    pub kind: String,
    /// 与 Python 完全同序的 query（不含 signature）
    pub query: String,
    pub signature: String,
    pub client_algo_id: String,
}

/// 单笔保护单的输入。
#[derive(Debug, Clone)]
pub struct LegSpec<'a> {
    pub symbol: &'a str,
    pub close_side: &'a str,
    pub kind: ProtectKind,
    /// 已按 `price_tick` 格式化好的触发价字符串（由调用方负责精度）
    pub trigger_price: &'a str,
    pub client_algo_id: &'a str,
    /// 双向持仓时的 `positionSide`（`LONG` / `SHORT`），单向为 None
    pub position_side: Option<&'a str>,
}

/// `quote_plus` 的逐字节复刻。
pub fn form_urlencode_pairs(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (idx, (k, v)) in pairs.iter().enumerate() {
        if idx > 0 {
            out.push('&');
        }
        encode_into(&mut out, k);
        out.push('=');
        encode_into(&mut out, v);
    }
    out
}

fn encode_into(out: &mut String, raw: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for b in raw.as_bytes() {
        match b {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(*b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
}

/// 构造并签名一笔保护单。
///
/// 参数顺序必须与 Python `_place_protection` 里 `algo_params` 的**插入顺序**一致：
/// algoType, symbol, side, type, triggerPrice, closePosition, workingType,
/// priceProtect, clientAlgoId, [positionSide]。
pub fn build_leg(signer: &Signer, spec: &LegSpec<'_>) -> ProtectionRequest {
    let mut pairs: Vec<(&str, &str)> = vec![
        ("algoType", "CONDITIONAL"),
        ("symbol", spec.symbol),
        ("side", spec.close_side),
        ("type", spec.kind.as_api_type()),
        ("triggerPrice", spec.trigger_price),
        ("closePosition", "true"),
        ("workingType", "MARK_PRICE"),
        ("priceProtect", "TRUE"),
        ("clientAlgoId", spec.client_algo_id),
    ];
    if let Some(ps) = spec.position_side {
        pairs.push(("positionSide", ps));
    }
    let query = form_urlencode_pairs(&pairs);
    ProtectionRequest {
        kind: spec.kind.as_api_type().to_string(),
        signature: signer.sign(&query),
        query,
        client_algo_id: spec.client_algo_id.to_string(),
    }
}

/// 并行构造 STOP + TP 两笔（`tokio::join!`，两者独立，无共享可变状态）。
pub async fn build_pair(
    signer: &Signer,
    stop: LegSpec<'_>,
    take: LegSpec<'_>,
) -> (ProtectionRequest, ProtectionRequest) {
    let (a, b) = tokio::join!(
        async { build_leg(signer, &stop) },
        async { build_leg(signer, &take) }
    );
    (a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_matches_python_quote_plus() {
        // Python: urlencode({"a":"x y","b":"A/B","c":"1.5"}) == "a=x+y&b=A%2FB&c=1.5"
        let got = form_urlencode_pairs(&[("a", "x y"), ("b", "A/B"), ("c", "1.5")]);
        assert_eq!(got, "a=x+y&b=A%2FB&c=1.5");
    }

    #[test]
    fn urlencode_keeps_unreserved_and_uppercases_hex() {
        let got = form_urlencode_pairs(&[("k", "a-b_c.d~e")]);
        assert_eq!(got, "k=a-b_c.d~e", "unreserved 不编码");
        let got2 = form_urlencode_pairs(&[("k", "中")]);
        assert_eq!(got2, "k=%E4%B8%AD", "非 ASCII 用大写 hex");
    }

    #[test]
    fn leg_query_order_matches_python() {
        let signer = Signer::detect("secret");
        let req = build_leg(
            &signer,
            &LegSpec {
                symbol: "BTCUSDT",
                close_side: "SELL",
                kind: ProtectKind::StopMarket,
                trigger_price: "73000",
                client_algo_id: "np123",
                position_side: None,
            },
        );
        assert_eq!(
            req.query,
            "algoType=CONDITIONAL&symbol=BTCUSDT&side=SELL&type=STOP_MARKET\
&triggerPrice=73000&closePosition=true&workingType=MARK_PRICE\
&priceProtect=TRUE&clientAlgoId=np123"
        );
        assert_eq!(req.kind, "STOP_MARKET");
        assert_eq!(signer.algo(), "hmac-sha256");
        // 64 字符 hex
        assert_eq!(req.signature.len(), 64);
    }

    #[test]
    fn hedge_mode_appends_position_side_last() {
        let signer = Signer::detect("secret");
        let req = build_leg(
            &signer,
            &LegSpec {
                symbol: "ETHUSDT",
                close_side: "BUY",
                kind: ProtectKind::TakeProfitMarket,
                trigger_price: "2500.5",
                client_algo_id: "np9",
                position_side: Some("SHORT"),
            },
        );
        assert!(req.query.ends_with("clientAlgoId=np9&positionSide=SHORT"));
    }

    #[tokio::test]
    async fn pair_is_built_in_parallel() {
        let signer = Signer::detect("secret");
        let mk = |kind, price: &'static str| LegSpec {
            symbol: "BTCUSDT",
            close_side: "SELL",
            kind,
            trigger_price: price,
            client_algo_id: "npX",
            position_side: None,
        };
        let (a, b) = build_pair(&signer, mk(ProtectKind::StopMarket, "73000"),
                                mk(ProtectKind::TakeProfitMarket, "75000")).await;
        assert!(a.query.contains("STOP_MARKET"));
        assert!(b.query.contains("TAKE_PROFIT_MARKET"));
        assert_ne!(a.signature, b.signature, "两笔价格不同，签名必须不同");
    }
}
