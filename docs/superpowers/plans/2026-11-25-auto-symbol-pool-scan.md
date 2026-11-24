# 自动标的池扫描实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 每 3 小时按 24h 成交额扫描前 300 个 USDT 永续，筛出量能与策略闸门都通过的标的，只替换原池中不达标且无持仓的标的，并补足到「每轮扫描标的数量」。

**架构：** 新增独立后台任务 `symbol_scan`，使用公开行情接口（无需 API 凭证）拉全市场 ticker/bookTicker/K 线；纯函数 `plan` 负责候选与替换决策，事务函数负责落库；不阻塞引擎主循环。目标池大小复用 `engine.max_symbols_per_cycle`，最小量比复用 `strategy.volume_ratio_min`，并发复用 `engine.scan_concurrency`。

**技术栈：** Rust（tokio、reqwest、rusqlite、futures-util）、Vue 3 + TypeScript、Vitest。

---

## 文件结构

| 文件 | 职责 |
|---|---|
| `rust/nanoquant/src/symbol_scan.rs`（新） | 配置读取、公开行情扫描、候选/替换计划、事务落库、后台循环 |
| `rust/nanoquant/src/providers.rs` | 新增公开行情方法：24h ticker、bookTicker、K 线 |
| `rust/nanoquant/src/symbols.rs` | 提取可复用的合约写入函数；新增 `/symbols/auto-scan` 与列表 `scan` 状态 |
| `rust/nanoquant/src/main.rs` | 启动独立 `symbol_scan` 后台任务 |
| `rust/nanoquant/src/default_settings.json` | 新增 `symbol_scan` 默认配置 |
| `rust/nanoquant/src/param_registry.rs` | `symbol_scan` 范围与可见性 |
| `rust/nanoquant/src/settings_api.rs` | 设置组白名单；`GET/PUT` 分组读写 |
| `rust/nanoquant/src/backup.rs` | 备份/还原复用同一设置组白名单 |
| `frontend/src/views/settingsCatalog.ts` | 新增「自动换池」分组与字段 |
| `frontend/src/views/StrategyView.vue` | 分组中文标签与说明 |
| `frontend/src/views/SymbolsView.vue` | 扫描状态、立即扫描按钮、计划确认弹窗 |
| `frontend/src/views/settingsCatalog.spec.ts` | 分组覆盖测试 |
| `docs/superpowers/specs/2026-11-25-auto-symbol-pool-scan-design.md` | 已批准规格 |

---

## 任务 1：新增 `symbol_scan` 配置分组

**文件：**
- 修改：`rust/nanoquant/src/default_settings.json`
- 修改：`rust/nanoquant/src/param_registry.rs`
- 修改：`rust/nanoquant/src/settings_api.rs`
- 修改：`rust/nanoquant/src/backup.rs`
- 修改：`frontend/src/views/settingsCatalog.ts`
- 修改：`frontend/src/views/StrategyView.vue`
- 测试：`frontend/src/views/settingsCatalog.spec.ts`

- [x] **步骤 1：写失败测试**

在 `settingsCatalog.spec.ts` 的 groupIds 断言中加入 `symbol_scan`：

```ts
expect(groupIds).toEqual([
  'engine',
  'strategy',
  'ai_tuning',
  'symbol_scan',
  'risk',
  'data',
  'news',
  'system',
  'backup',
  'security',
])
```

- [x] **步骤 2：运行测试确认失败**

运行：`cd frontend && npm run test -- settingsCatalog`
预期：FAIL，`symbol_scan` 不在分组列表中。

- [x] **步骤 3：实现配置**

`default_settings.json` 增加：

```json
"symbol_scan": {
  "enabled": true,
  "interval_hours": 3,
  "market_limit": 300
}
```

`param_registry.rs` 的 `RANGES` 增加：

```rust
("symbol_scan", "interval_hours", 1.0, 24.0, 1.0),
("symbol_scan", "market_limit", 50.0, 500.0, 10.0),
```

`VISIBLE` 增加：

```rust
(
    "symbol_scan",
    &["enabled", "interval_hours", "market_limit"],
),
```

`AI_BLOCKED_GROUPS` 改为：

```rust
const AI_BLOCKED_GROUPS: &[&str] = &["ai_tuning", "risk", "symbol_scan"];
```

`settingsCatalog.ts` 增加：

```ts
{ id: 'symbol_scan', label: '自动换池', hint: '每 3 小时按成交额扫描前 300 个合约，自动替换不达标且无持仓的标的' },
```

```ts
symbol_scan: [
  { title: '扫描', fields: ['enabled', 'interval_hours', 'market_limit'] },
],
```

`StrategyView.vue` 的 `GROUP_META` 增加：

```ts
symbol_scan: {
  enabled: { label: '自动换池', widget: 'switch', hint: '开启后按间隔自动扫描并替换标的' },
  interval_hours: { label: '扫描间隔', unit: '小时', hint: '1–24 小时；默认 3 小时' },
  market_limit: { label: '扫描数量', unit: '个', hint: '按 24h 成交额取前 N 个 USDT 永续' },
},
```

- [x] **步骤 4：运行测试确认通过**

运行：`cd frontend && npm run test -- settingsCatalog`
预期：PASS。

- [x] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/default_settings.json rust/nanoquant/src/param_registry.rs frontend/src/views/settingsCatalog.ts frontend/src/views/settingsCatalog.spec.ts frontend/src/views/StrategyView.vue
git commit -m "feat(symbol-scan): 新增自动换池配置分组"
```

已由 581a26a、29247fa 及本次修复提交完成

---

## 任务 2：Provider 公开行情方法

**文件：**
- 修改：`rust/nanoquant/src/providers.rs`
- 测试：`rust/nanoquant/src/providers.rs`（`mod tests`）

- [ ] **步骤 1：写失败测试**

```rust
#[test]
fn parses_full_market_tickers_and_book_tops() {
    let tickers = parse_tickers_24h(&serde_json::json!([
        {"symbol": "BTCUSDT", "quoteVolume": "12345.6"}
    ]))
    .unwrap();
    assert_eq!(tickers[0].symbol, "BTCUSDT");
    assert_eq!(tickers[0].quote_volume, 12345.6);

    let tops = parse_book_tops(&serde_json::json!([
        {"symbol": "BTCUSDT", "bidPrice": "100", "askPrice": "101"}
    ]))
    .unwrap();
    assert_eq!(tops[0].symbol, "BTCUSDT");
    assert!((tops[0].spread_bps - 99.0).abs() < 1.0);
}
```

- [ ] **步骤 2：运行确认失败**

运行：`cargo test --bin nanoquant parses_full_market_tickers`
预期：FAIL，函数不存在。

- [ ] **步骤 3：实现**

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct ExchangeTicker {
    pub symbol: String,
    pub quote_volume: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BookTop {
    pub symbol: String,
    pub bid: f64,
    pub ask: f64,
    pub spread_bps: f64,
}

pub fn parse_tickers_24h(value: &Value) -> Result<Vec<ExchangeTicker>, ProviderError> {
    let rows = value
        .as_array()
        .ok_or_else(|| ProviderError::InvalidData("24h 行情响应不是数组".into()))?;
    rows.iter()
        .map(|row| {
            Ok(ExchangeTicker {
                symbol: row["symbol"]
                    .as_str()
                    .ok_or_else(|| ProviderError::InvalidData("24h 行情缺少 symbol".into()))?
                    .to_string(),
                quote_volume: number(row, "quoteVolume")?,
            })
        })
        .collect()
}

pub fn parse_book_tops(value: &Value) -> Result<Vec<BookTop>, ProviderError> {
    let rows = value
        .as_array()
        .ok_or_else(|| ProviderError::InvalidData("盘口响应不是数组".into()))?;
    rows.iter()
        .map(|row| {
            let symbol = row["symbol"].as_str().unwrap_or_default().to_string();
            let bid = number(row, "bidPrice")?;
            let ask = number(row, "askPrice")?;
            let mid = (bid + ask) / 2.0;
            let spread_bps = if mid > 0.0 {
                ((ask - bid) / mid * 10_000.0).max(0.0)
            } else {
                f64::INFINITY
            };
            Ok(BookTop { symbol, bid, ask, spread_bps })
        })
        .collect()
}
```

`impl BinanceProvider` 增加：

```rust
/// 公开行情专用：不要求 API 凭证。
pub fn public(base_url: impl Into<String>) -> Result<Self, ProviderError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| ProviderError::Unavailable(format!("行情客户端不可用: {error}")))?;
    Ok(Self {
        client,
        base_url: base_url.into(),
        symbols: Vec::new(),
        api_key: String::new(),
        secret: String::new(),
        timeframe: "4h".into(),
        public_book: None,
    })
}

pub async fn tickers_24h(&self) -> Result<Vec<ExchangeTicker>, ProviderError> {
    let value = self.get_json("/fapi/v1/ticker/24hr", &[], false).await?;
    parse_tickers_24h(&value)
}

pub async fn book_tops(&self) -> Result<Vec<BookTop>, ProviderError> {
    let value = self.get_json("/fapi/v1/ticker/bookTicker", &[], false).await?;
    parse_book_tops(&value)
}

pub async fn klines(&self, symbol: &str, limit: usize) -> Result<Vec<CandleSnapshot>, ProviderError> {
    let params = vec![
        ("symbol".to_string(), symbol.to_string()),
        ("interval".to_string(), self.timeframe.clone()),
        ("limit".to_string(), limit.to_string()),
    ];
    let value = self.get_json("/fapi/v1/klines", &params, false).await?;
    parse_closed_candles(&value, crate::now_millis())
}
```

- [ ] **步骤 4：运行确认通过**

运行：`cargo test --bin nanoquant parses_full_market_tickers`
预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/providers.rs
git commit -m "feat(symbol-scan): 增加全市场行情与 K 线公开读取"
```

---

## 任务 3：标的写入函数复用

**文件：**
- 修改：`rust/nanoquant/src/symbols.rs`
- 测试：`rust/nanoquant/src/symbols.rs`（`mod tests`）

- [ ] **步骤 1：写失败测试**

```rust
#[test]
fn upsert_contract_row_inserts_known_contract() {
    let db = seed(&std::env::temp_dir().join("nq-symbol-scan-upsert.db"));
    let contracts = vec![contract("BTCUSDT", "PERPETUAL", "TRADING", "USDT")];
    let row = upsert_contract_row(&db, &contracts, "BTCUSDT").unwrap();
    assert_eq!(row["symbol"], "BTCUSDT");
    let count: i64 = db
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM trading_symbols", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}
```

- [ ] **步骤 2：运行确认失败**

运行：`cargo test --bin nanoquant upsert_contract_row`
预期：FAIL，函数不存在。

- [ ] **步骤 3：实现**

从 `add_with_contracts` 提取校验、`build_pool_row`、note 计算和 INSERT：

```rust
pub(crate) fn upsert_contract_row(
    db: &Db,
    contracts: &[Value],
    symbol: &str,
) -> Result<Value, String> {
    if !plausible_symbol(symbol) {
        return Err("标的名称无效".into());
    }
    let raw = contracts
        .iter()
        .find(|row| row["symbol"].as_str() == Some(symbol))
        .ok_or_else(|| format!("{symbol} 不在币安合约表中"))?;
    if !valid_contract(raw, symbol) {
        return Err("仅允许当前可交易的 USDT 永续合约".into());
    }
    let mut row = build_pool_row(raw, leverage_cap(db))
        .ok_or_else(|| "币安合约规格无效".to_string())?;
    let note = if raw["underlyingType"].as_str().unwrap_or("").to_uppercase() == "EQUITY"
        || raw["underlyingType"].as_str().unwrap_or("").to_uppercase() == "COMMODITY"
    {
        "股票/商品类永续".to_string()
    } else {
        String::new()
    };
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let order: i64 = conn
        .query_row("SELECT COALESCE(MAX(sort_order),0)+1 FROM trading_symbols", [], |r| r.get(0))
        .unwrap_or(1);
    conn.execute(
        "INSERT INTO trading_symbols(symbol,name,tier,sl_atr_mult,enabled,max_leverage,precision,tick_sz,min_sz,min_notional,ct_val,min_score,note,sort_order,created_at,updated_at) \
         VALUES(?1,?2,?3,?4,1,?5,?6,?7,?8,?9,1,0,?10,?11,?12,?12)",
        rusqlite::params![
            symbol,
            row["name"].as_str().unwrap_or(symbol),
            row["tier"].as_str().unwrap_or("tier_2_momentum"),
            row["sl_atr_mult"].as_f64().unwrap_or(2.2),
            row["max_leverage"].as_i64().unwrap_or(20),
            row["precision"].as_i64().unwrap_or(3),
            row["tick_sz"].as_str().unwrap_or("0.0001"),
            row["min_sz"].as_str().unwrap_or("0.001"),
            row["min_notional"].as_f64().unwrap_or(5.0),
            note,
            order,
            crate::now(),
        ],
    )
    .map_err(|_| "标的写入失败".to_string())?;
    row["enabled"] = json!(true);
    row["custom"] = json!(false);
    Ok(row)
}
```

`add_with_contracts` 改为先做 pool 上限/重复检查，再调用 `upsert_contract_row`，保留原有返回结构。

同时提供连接版供事务复用：

```rust
pub(crate) fn upsert_contract_row_conn(
    conn: &rusqlite::Connection,
    contracts: &[Value],
    symbol: &str,
) -> Result<Value, String> {
    if !plausible_symbol(symbol) {
        return Err("标的名称无效".into());
    }
    let raw = contracts
        .iter()
        .find(|row| row["symbol"].as_str() == Some(symbol))
        .ok_or_else(|| format!("{symbol} 不在币安合约表中"))?;
    if !valid_contract(raw, symbol) {
        return Err("仅允许当前可交易的 USDT 永续合约".into());
    }
    let cap = conn
        .query_row("SELECT value FROM app_settings WHERE key='risk'", [], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|risk| risk["max_leverage"].as_i64())
        .unwrap_or(20)
        .clamp(1, 100);
    let mut row = build_pool_row(raw, cap).ok_or_else(|| "币安合约规格无效".to_string())?;
    let note = if raw["underlyingType"].as_str().unwrap_or("").to_uppercase() == "EQUITY"
        || raw["underlyingType"].as_str().unwrap_or("").to_uppercase() == "COMMODITY"
    {
        "股票/商品类永续".to_string()
    } else {
        String::new()
    };
    let order: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(sort_order),0)+1 FROM trading_symbols",
            [],
            |row| row.get(0),
        )
        .unwrap_or(1);
    conn.execute(
        "INSERT INTO trading_symbols(symbol,name,tier,sl_atr_mult,enabled,max_leverage,precision,tick_sz,min_sz,min_notional,ct_val,min_score,note,sort_order,created_at,updated_at) \
         VALUES(?1,?2,?3,?4,1,?5,?6,?7,?8,?9,1,0,?10,?11,?12,?12)",
        rusqlite::params![
            symbol,
            row["name"].as_str().unwrap_or(symbol),
            row["tier"].as_str().unwrap_or("tier_2_momentum"),
            row["sl_atr_mult"].as_f64().unwrap_or(2.2),
            row["max_leverage"].as_i64().unwrap_or(20),
            row["precision"].as_i64().unwrap_or(3),
            row["tick_sz"].as_str().unwrap_or("0.0001"),
            row["min_sz"].as_str().unwrap_or("0.001"),
            row["min_notional"].as_f64().unwrap_or(5.0),
            note,
            order,
            crate::now(),
        ],
    )
    .map_err(|_| "标的写入失败".to_string())?;
    row["enabled"] = json!(true);
    row["custom"] = json!(false);
    Ok(row)
}
```

`upsert_contract_row(db, ...)` 改为加锁后调用 `upsert_contract_row_conn`。

- [ ] **步骤 4：运行确认通过**

运行：`cargo test --bin nanoquant upsert_contract_row`
预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/symbols.rs
git commit -m "refactor(symbol-scan): 提取可复用标的写入函数"
```

---

## 任务 4：纯函数替换计划

**文件：**
- 创建：`rust/nanoquant/src/symbol_scan.rs`
- 修改：`rust/nanoquant/src/main.rs`（`mod symbol_scan;`）
- 测试：`rust/nanoquant/src/symbol_scan.rs`（`mod tests`）

- [ ] **步骤 1：写失败测试**

```rust
#[test]
fn replaces_only_failed_unheld_symbols_and_fills_to_target() {
    let current = vec!["AAAUSDT".into(), "BBBUSDT".into(), "CCCUSDT".into()];
    let held: HashSet<String> = HashSet::new();
    let scanned = vec![
        scanned("AAAUSDT", 0.2), // 不达标
        scanned("BBBUSDT", 1.0), // 达标
        scanned("CCCUSDT", 0.1), // 不达标
        scanned("DDDUSDT", 2.0), // 候选
        scanned("EEEUSDT", 1.5), // 候选
    ];
    let plan = plan(&current, &held, &scanned, &gates(), 3);
    assert_eq!(plan.delete, vec!["AAAUSDT", "CCCUSDT"]);
    assert_eq!(plan.add, vec!["DDDUSDT", "EEEUSDT"]);
    assert_eq!(plan.keep, vec!["BBBUSDT"]);
}
```

- [ ] **步骤 2：运行确认失败**

运行：`cargo test --bin nanoquant replaces_only_failed_unheld_symbols`
预期：FAIL，模块/函数不存在。

- [ ] **步骤 3：实现**

```rust
use std::collections::{HashMap, HashSet};

use axum::Json;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::scan::{risk_gate, Candidate, ScanGates};
```

测试覆盖：

- 全部通过：不替换。
- 持仓不达标：保留。
- 候选不足：只替换有候选的。
- 补足到 target。
- 原池超过 target：不裁减合格。
- 原池 ATR/成交额/点差不通过：进入待替换。

- [ ] **步骤 4：运行确认通过**

运行：`cargo test --bin nanoquant symbol_scan::tests`
预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/symbol_scan.rs rust/nanoquant/src/main.rs
git commit -m "feat(symbol-scan): 候选与替换计划纯函数"
```

---

## 任务 5：扫描行情与事务落库

**文件：**
- 修改：`rust/nanoquant/src/symbol_scan.rs`
- 测试：`rust/nanoquant/src/symbol_scan.rs`（`mod tests`）

- [ ] **步骤 1：写失败测试**

```rust
#[test]
fn apply_plan_keeps_held_symbols_and_replaces_the_rest() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE trading_symbols(symbol TEXT PRIMARY KEY, enabled INTEGER NOT NULL);
         CREATE TABLE positions(symbol TEXT, status TEXT, mode TEXT);
         INSERT INTO trading_symbols VALUES('AAAUSDT',1);
         INSERT INTO trading_symbols VALUES('BBBUSDT',1);
         INSERT INTO positions VALUES('BBBUSDT','open','live');",
    )
    .unwrap();
    let plan = ScanPlan {
        keep: vec!["BBBUSDT".into()],
        delete: vec!["AAAUSDT".into()],
        add: vec!["CCCUSDT".into()],
        held: vec!["BBBUSDT".into()],
        candidates: vec!["CCCUSDT".into()],
    };
    apply_plan(&conn, "live", &plan, &[]).unwrap();
    let symbols: Vec<String> = conn
        .prepare("SELECT symbol FROM trading_symbols ORDER BY symbol")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(symbols, vec!["BBBUSDT", "CCCUSDT"]);
}
```

- [ ] **步骤 2：运行确认失败**

运行：`cargo test --bin nanoquant apply_plan_keeps_held`
预期：FAIL，`apply_plan` 不存在。

- [ ] **步骤 3：实现**

```rust
pub async fn run(db: &crate::db::Db, apply: bool) -> Result<Value, String> {
    let cfg = load_config(db)?;
    if cfg.market_limit == 0 {
        return Err("扫描数量无效".into());
    }
    let mode = crate::symbols::current_mode(db);
    let contracts = crate::symbols::exchange_contracts(db)
        .await
        .map_err(|(_, Json(body))| {
            body["detail"]
                .as_str()
                .unwrap_or("币安合约表读取失败")
                .to_string()
        })?;
    let provider = crate::providers::BinanceProvider::public(cfg.base_url.clone())
        .map_err(|error| format!("行情客户端不可用: {error:?}"))?;
    let current = current_pool(db)?;
    let current_set: HashSet<String> = current.iter().cloned().collect();
    let tickers = provider
        .tickers_24h()
        .await
        .map_err(|error| format!("24h 行情失败: {error:?}"))?;
    let books = provider
        .book_tops()
        .await
        .map_err(|error| format!("盘口失败: {error:?}"))?;
    let tradable: HashSet<String> = contracts
        .iter()
        .filter(|row| {
            row["contractType"].as_str() == Some("PERPETUAL")
                && row["status"].as_str() == Some("TRADING")
                && row["quoteAsset"].as_str() == Some("USDT")
        })
        .filter_map(|row| row["symbol"].as_str().map(str::to_string))
        .collect();
    let ticker_map: std::collections::HashMap<String, f64> = tickers
        .iter()
        .map(|row| (row.symbol.clone(), row.quote_volume))
        .collect();
    let mut ranked: Vec<_> = tickers
        .iter()
        .filter(|row| tradable.contains(&row.symbol))
        .filter(|row| row.quote_volume > 0.0)
        .cloned()
        .collect();
    ranked.sort_by(|a, b| {
        b.quote_volume
            .partial_cmp(&a.quote_volume)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked.truncate(cfg.market_limit);
    // 原池即使不在成交额前 300 也要参与判断，否则会被误判为不达标。
    let mut scan_symbols: Vec<String> = ranked.iter().map(|row| row.symbol.clone()).collect();
    for symbol in &current {
        if !scan_symbols.contains(symbol) {
            scan_symbols.push(symbol.clone());
        }
    }
    let book_map: std::collections::HashMap<String, f64> =
        books.into_iter().map(|row| (row.symbol, row.spread_bps)).collect();

    let mut scanned = Vec::new();
    for chunk in scan_symbols.chunks(cfg.concurrency.max(1)) {
        let mut tasks = tokio::task::JoinSet::new();
        for symbol in chunk {
            let provider = provider.clone();
            let symbol = symbol.clone();
            let kline_limit = cfg.kline_limit;
            let atr_period = cfg.atr_period;
            let quote_volume = ticker_map.get(&symbol).copied().unwrap_or(0.0);
            let spread_bps = book_map.get(&symbol).copied().unwrap_or(f64::INFINITY);
            tasks.spawn(async move {
                let candles = provider.klines(&symbol, kline_limit).await.ok();
                let metrics = candles
                    .as_deref()
                    .and_then(|rows| crate::providers::candle_metrics(rows, atr_period));
                ScannedSymbol {
                    symbol,
                    data_ok: metrics.is_some(),
                    vol_ratio: metrics.map(|m| m.vol_ratio).unwrap_or(0.0),
                    atr_pct: metrics.map(|m| m.atr_pct).unwrap_or(f64::INFINITY),
                    quote_volume,
                    spread_bps,
                }
            });
        }
        while let Some(Ok(row)) = tasks.join_next().await {
            scanned.push(row);
        }
    }

    let held = held_symbols(db, &mode)?;
    let plan = plan(&current, &held, &scanned, &cfg.gates, cfg.target);
    let result = json!({
        "scanned": scanned.len(),
        "candidates": plan.candidates,
        "keep": plan.keep,
        "held": plan.held,
        "delete": plan.delete,
        "add": plan.add,
        "applied": apply,
    });
    if apply {
        let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        apply_plan(&tx, &mode, &plan, &contracts)?;
        tx.execute(
            "INSERT INTO engine_state(key,value,updated_at) VALUES('symbol_scan.last_run_ts',?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
            (crate::now().to_string(), crate::now()),
        ).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO engine_state(key,value,updated_at) VALUES('symbol_scan.last_result',?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
            (result.to_string(), crate::now()),
        ).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO engine_events(ts,level,category,message,payload) \
             VALUES(?1,'info','symbol_scan',?2,?3)",
            (
                crate::now(),
                format!(
                    "自动换池：替换 {} 个，新增 {} 个",
                    plan.delete.len(),
                    plan.add.len()
                ),
                result.to_string(),
            ),
        ).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
    }
    Ok(result)
}

fn apply_plan(
    conn: &Connection,
    mode: &str,
    plan: &ScanPlan,
    contracts: &[Value],
) -> Result<(), String> {
    for symbol in &plan.delete {
        let held: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM positions WHERE symbol=?1 AND mode=?2 AND status='open')",
                (symbol, mode),
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if held {
            continue;
        }
        conn.execute("DELETE FROM trading_symbols WHERE symbol=?1", [symbol])
            .map_err(|e| e.to_string())?;
    }
    for symbol in &plan.add {
        if contracts.is_empty() {
            continue;
        }
        crate::symbols::upsert_contract_row_conn(conn, contracts, symbol)?;
    }
    Ok(())
}
```

`load_config` 读取 `symbol_scan`、`engine`、`strategy`、当前 mode 与 `data` 域名；`current_pool`/`held_symbols` 为 SQL 辅助函数；`upsert_contract_row_conn` 为 `symbols.rs` 的连接版（任务 3 提供）。

```rust
#[derive(Debug, Clone)]
struct Config {
    enabled: bool,
    interval_hours: i64,
    market_limit: usize,
    target: usize,
    concurrency: usize,
    kline_limit: usize,
    atr_period: usize,
    gates: ScanGates,
    base_url: String,
}

fn load_config(db: &Db) -> Result<Config, String> {
    let scan = db.setting("symbol_scan").ok().flatten().unwrap_or_else(|| json!({}));
    let engine = db.setting("engine").ok().flatten().unwrap_or_else(|| json!({}));
    let strategy = db.setting("strategy").ok().flatten().unwrap_or_else(|| json!({}));
    let data = db.setting("data").ok().flatten().unwrap_or_else(|| json!({}));
    let mode = crate::symbols::current_mode(db);
    let base_url = if mode == "live" {
        data["live_base_url"].as_str().unwrap_or("https://fapi.binance.com").to_string()
    } else {
        data["demo_base_url"].as_str().unwrap_or("https://testnet.binancefuture.com").to_string()
    };
    let spread_bps_max = strategy["max_spread_bps"]
        .as_f64()
        .or_else(|| strategy["spread_bps_max"].as_f64())
        .unwrap_or(8.0);
    Ok(Config {
        enabled: scan["enabled"].as_bool().unwrap_or(true),
        interval_hours: scan["interval_hours"].as_i64().unwrap_or(3).clamp(1, 24),
        market_limit: scan["market_limit"].as_u64().unwrap_or(300).clamp(50, 500) as usize,
        target: engine["max_symbols_per_cycle"].as_u64().unwrap_or(12).clamp(1, 100) as usize,
        concurrency: engine["scan_concurrency"].as_u64().unwrap_or(3).clamp(1, 20) as usize,
        kline_limit: data["kline_limit"].as_u64().unwrap_or(200).clamp(20, 500) as usize,
        atr_period: strategy["atr_period"].as_u64().unwrap_or(14) as usize,
        gates: ScanGates {
            atr_pct_min: strategy["atr_pct_min"].as_f64().unwrap_or(0.05),
            atr_pct_max: strategy["atr_pct_max"].as_f64().unwrap_or(4.0),
            volume_ratio_min: strategy["volume_ratio_min"].as_f64().unwrap_or(0.8),
            min_24h_quote_volume: strategy["min_24h_quote_volume"].as_f64().unwrap_or(3_000_000.0),
            spread_bps_max,
            entry_threshold: strategy["entry_threshold"].as_f64().unwrap_or(0.3),
        },
        base_url,
    })
}

fn current_pool(db: &Db) -> Result<Vec<String>, String> {
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let mut stmt = conn
        .prepare("SELECT symbol FROM trading_symbols WHERE enabled=1 ORDER BY sort_order, symbol")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

fn held_symbols(db: &Db, mode: &str) -> Result<HashSet<String>, String> {
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let mut stmt = conn
        .prepare("SELECT DISTINCT symbol FROM positions WHERE mode=?1 AND status='open'")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([mode], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    Ok(rows.filter_map(Result::ok).collect())
}

fn last_run_ts(db: &Db) -> Result<i64, String> {
    use rusqlite::OptionalExtension;
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key='symbol_scan.last_run_ts'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(value.and_then(|raw| raw.parse::<i64>().ok()).unwrap_or(0))
}

fn set_last_run_ts(db: &Db, ts: i64) -> Result<(), String> {
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    conn.execute(
        "INSERT INTO engine_state(key,value,updated_at) VALUES('symbol_scan.last_run_ts',?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
        (ts.to_string(), crate::now()),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
```

`symbols.rs` 中把 `current_mode` 与 `exchange_contracts` 改为 `pub(crate)`。

- [ ] **步骤 4：运行确认通过**

运行：`cargo test --bin nanoquant apply_plan_keeps_held`
预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/symbol_scan.rs rust/nanoquant/src/symbols.rs
git commit -m "feat(symbol-scan): 行情扫描与事务替换"
```

---

## 任务 6：独立后台循环与启动接入

**文件：**
- 修改：`rust/nanoquant/src/symbol_scan.rs`
- 修改：`rust/nanoquant/src/main.rs`
- 测试：`rust/nanoquant/src/symbol_scan.rs`

- [ ] **步骤 1：写失败测试**

```rust
#[test]
fn first_run_is_delayed_until_interval_elapsed() {
    let now = 1_000_000;
    assert!(!due(0, now, 3));
    assert!(due(now - 3 * 3600, now, 3));
    assert!(!due(now - 2 * 3600, now, 3));
}
```

- [ ] **步骤 2：运行确认失败**

运行：`cargo test --bin nanoquant first_run_is_delayed`
预期：FAIL。

- [ ] **步骤 3：实现**

```rust
pub fn due(last_run_ts: i64, now: i64, interval_hours: i64) -> bool {
    last_run_ts > 0 && now.saturating_sub(last_run_ts) >= interval_hours.max(1) * 3600
}

pub fn spawn_loop(
    db: crate::db::Db,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if *stop.borrow() {
                break;
            }
            let cfg = load_config(&db).ok();
            if let Some(cfg) = cfg {
                if cfg.enabled {
                    let last = last_run_ts(&db).unwrap_or(0);
                    let now = crate::now();
                    if last == 0 {
                        let _ = set_last_run_ts(&db, now);
                    } else if due(last, now, cfg.interval_hours) {
                        if let Err(error) = run(&db, true).await {
                            tracing::warn!(%error, "自动换池失败");
                        }
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {},
                _ = stop.changed() => break,
            }
        }
    })
}
```

`main.rs` 在 `let (stop_tx, stop_rx) = runtime::stop_channel();` 后加入：

```rust
if let Some(db) = database.clone() {
    tokio::spawn(crate::symbol_scan::spawn_loop(db, stop_tx.subscribe()));
}
```

- [ ] **步骤 4：运行确认通过**

运行：`cargo test --bin nanoquant first_run_is_delayed`
预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/symbol_scan.rs rust/nanoquant/src/main.rs
git commit -m "feat(symbol-scan): 接入独立后台扫描任务"
```

---

## 任务 7：API 与列表状态

**文件：**
- 修改：`rust/nanoquant/src/symbols.rs`
- 测试：`rust/nanoquant/src/symbols.rs`

- [ ] **步骤 1：写失败测试**

```rust
#[tokio::test]
async fn list_symbols_includes_scan_state() {
    let db = seed(&std::env::temp_dir().join("nq-symbols-scan-state.db"));
    let app = Router::new().nest("/api/v1", routes()).with_state(db);
    let response = app
        .oneshot(Request::builder().uri("/api/v1/symbols").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap();
    assert!(body["scan"].is_object());
}
```

- [ ] **步骤 2：运行确认失败**

运行：`cargo test --bin nanoquant list_symbols_includes_scan_state`
预期：FAIL，缺少 `scan`。

- [ ] **步骤 3：实现**

`routes()` 增加：

```rust
.route("/symbols/auto-scan", post(auto_scan_symbols))
```

```rust
#[derive(Deserialize, Default)]
struct AutoScanInput {
    #[serde(default)]
    apply: bool,
}

async fn auto_scan_symbols(
    State(db): State<Db>,
    Json(input): Json<AutoScanInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    crate::symbol_scan::run(&db, input.apply)
        .await
        .map(Json)
        .map_err(|error| pool_error(StatusCode::BAD_GATEWAY, &error))
}
```

`list_symbols` 的 JSON 增加：

```rust
let scan_last: Option<String> = conn
    .query_row(
        "SELECT value FROM engine_state WHERE key='symbol_scan.last_run_ts'",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| e.to_string())?;
let scan_result: Option<String> = conn
    .query_row(
        "SELECT value FROM engine_state WHERE key='symbol_scan.last_result'",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| e.to_string())?;
```

```rust
"scan": {
    "last_run_ts": scan_last
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(0),
    "last_result": scan_result
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null),
}
```

- [ ] **步骤 4：运行确认通过**

运行：`cargo test --bin nanoquant list_symbols_includes_scan_state`
预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/symbols.rs
git commit -m "feat(symbol-scan): 增加手动扫描 API 与列表状态"
```

---

## 任务 8：前端标的池扫描入口

**文件：**
- 修改：`frontend/src/views/SymbolsView.vue`
- 测试：`frontend/src/views/settingsCatalog.spec.ts`（已在任务 1 覆盖）

- [ ] **步骤 1：实现状态与请求**

```ts
interface ScanState {
  last_run_ts: number
  last_result: any
}

const scan = ref<ScanState | null>(null)
const scanning = ref(false)
const scanPlan = ref<any>(null)
```

`loadPool()` 增加：

```ts
scan.value = res.scan || null
```

```ts
async function runScan(apply: boolean) {
  scanning.value = true
  try {
    const res = await http.post('/symbols/auto-scan', { apply })
    scanPlan.value = res
    if (apply) {
      scan.value = { last_run_ts: Math.floor(Date.now() / 1000), last_result: res }
      await loadPool()
      app.notify(`已替换 ${res.delete?.length || 0} 个标的`, 'ok')
    }
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    scanning.value = false
  }
}
```

- [ ] **步骤 2：模板**

在标的池页头增加：

```vue
<button class="btn" :disabled="scanning" @click="runScan(false)">立即扫描</button>
<div v-if="scan" class="hint">
  上次扫描：{{ scan.last_run_ts ? timeStr(scan.last_run_ts, false) : '无' }}
  <span v-if="scan.last_result"> · 替换 {{ scan.last_result.delete?.length || 0 }} 个</span>
</div>
```

`SymbolsView.vue` 的 import 改为：

```ts
import { fmt, timeStr } from '../utils/format'
```

计划确认弹窗：复用现有 `Modal`，展示 `scanPlan.delete`、`scanPlan.add`、`scanPlan.keep`、`scanPlan.held`，确认按钮调用 `runScan(true)`。

- [ ] **步骤 3：验证**

运行：

```bash
cd frontend
npm run typecheck
npm run test
npm run build
```

预期：全部通过。

- [ ] **步骤 4：Commit**

```bash
git add frontend/src/views/SymbolsView.vue
git commit -m "feat(symbol-scan): 标的池页增加扫描状态与立即扫描"
```

---

## 任务 9：全量验证

- [ ] **步骤 1：Rust 全量测试**

运行：`cd rust/nanoquant && cargo test --bin nanoquant -- --test-threads=2`
预期：全部通过（不使用已重复执行的无参数命令）。

- [ ] **步骤 2：clippy**

运行：`cd rust/nanoquant && cargo clippy --bin nanoquant -- -D warnings`
预期：通过。

- [ ] **步骤 3：前端**

运行：`cd frontend && npm run typecheck && npm run test && npm run build`
预期：通过。

- [ ] **步骤 4：推送前检查**

确认 `symbol_scan` 后台任务与 API 已落地，否则不要推送。

- [ ] **步骤 5：提交推送**

```bash
git add -A
git commit -m "feat(symbol-scan): 接入自动标的池扫描"
git push origin main
```

---

## 规格覆盖自检

| 规格需求 | 任务 |
|---|---|
| 默认开启、3 小时间隔、前 300 | 任务 1、6 |
| 量能比 + ATR + 成交额 + 点差 | 任务 4 |
| 原池完整闸门、有持仓保留 | 任务 4、5 |
| 只替换不达标数量、补足目标 | 任务 4 |
| 候选不足不强删、超目标不裁减 | 任务 4 |
| 独立后台任务、不阻塞引擎 | 任务 6 |
| 立即扫描 dry-run / apply | 任务 7、8 |
| 状态与事件 | 任务 5、7、8 |
| 失败回滚 | 任务 5 |

## 占位符检查

计划中没有 TODO、待定或未定义的函数；`ScannedSymbol`、`ScanPlan`、`plan`、`apply_plan`、`spawn_loop`、`due` 在首次定义后保持一致。