//! 自进化：心法注入。对齐 `evolution/mantra.py` 的 `strip_leverage_hint`。
//!
//! 心法正文里的杠杆倍数（30x、20倍）注入给模型后会被照抄，而宿主已经按
//! 全局上下限钳制过杠杆，照抄等于把钳好的值带偏。只剥这类表述，
//! R / ATR / 百分比等分析性数值原样保留。

/// 只匹配「杠杆」与「N倍 / Nx」同时出现的表述，替换成中性提示。
pub fn strip_leverage_hint(rule: &str) -> String {
    let mut out = String::with_capacity(rule.len());
    let chars: Vec<char> = rule.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if let Some(end) = leverage_span(&chars, i) {
            out.push_str("杠杆(按风控区间)");
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn starts_with_word(chars: &[char], i: usize, word: &str) -> bool {
    let w: Vec<char> = word.chars().collect();
    chars[i..].len() >= w.len()
        && chars[i..i + w.len()]
            .iter()
            .zip(&w)
            .all(|(a, b)| a.to_lowercase().next() == b.to_lowercase().next())
}

fn is_unit(c: char) -> bool {
    matches!(c, '倍' | 'x' | 'X' | '×')
}

/// 从 i 起是否是一段杠杆倍数表述，返回其结束位置。
fn leverage_span(chars: &[char], i: usize) -> Option<usize> {
    // 「杠杆/leverage」在前，数字在后
    for word in ["杠杆", "leverage"] {
        if !starts_with_word(chars, i, word) {
            continue;
        }
        let mut k = i + word.chars().count();
        if k < chars.len() && matches!(chars[k], ':' | '：') {
            k += 1;
        }
        while k < chars.len() && chars[k].is_whitespace() {
            k += 1;
        }
        let start = k;
        while k < chars.len() && chars[k].is_ascii_digit() && k - start < 3 {
            k += 1;
        }
        if k == start {
            return None;
        }
        if k < chars.len() && is_unit(chars[k]) {
            k += 1;
        }
        return Some(k);
    }
    // 数字在前：1..3 位 + 倍|x|× + 杠杆
    let mut k = i;
    while k < chars.len() && chars[k].is_ascii_digit() && k - i < 3 {
        k += 1;
    }
    if k == i || k >= chars.len() || !is_unit(chars[k]) {
        return None;
    }
    k += 1;
    while k < chars.len() && chars[k].is_whitespace() {
        k += 1;
    }
    if starts_with_word(chars, k, "杠杆") {
        return Some(k + "杠杆".chars().count());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leverage_numbers_are_stripped_but_analysis_numbers_stay() {
        let a = strip_leverage_hint("信号强时可用 30x 杠杆");
        assert!(!a.contains("30x"), "杠杆倍数不能进 prompt");
        assert!(a.contains("杠杆(按风控区间)"));

        let b = strip_leverage_hint("用 20倍杠杆进场");
        assert!(!b.contains("20倍"));

        let c = strip_leverage_hint("仓位用 5x 杠杆，止损 2%");
        assert!(c.contains("2%"), "止损百分比是分析性数值，必须保留");

        let d = strip_leverage_hint("回踩 1.5 ATR 且不破 EMA21");
        assert_eq!(d, "回踩 1.5 ATR 且不破 EMA21", "没有杠杆表述时原文不动");
    }
}
