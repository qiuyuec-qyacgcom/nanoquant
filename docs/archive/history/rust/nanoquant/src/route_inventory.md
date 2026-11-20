> 历史记录：已退役，不可执行
> 源路径：rust/nanoquant/src/route_inventory.md
> 归档时间：2026-11-20T09:17:25Z

# Python / Rust API 路由差异清单

生成日期：2026-11-19。此清单仅涵盖本轮已核实的优先缺口，不是完整路由 inventory。判断来源：Python `backend/nanoquant/api/routes/*.py` decorators；Rust `rust/nanoquant/src/main.rs` 及对应 `*_api.rs` 的 route 注册和 handler。

| 优先级 | 方法 / 路径 | Python | Rust | 状态 / 建议 |
|---|---|---|---|---|
| P0 | GET `/system/runtime` | `system.py` 使用 50ms RuntimeProbe，返回 uptime、samples、loop lag 百分位、GC、timer note、长阻塞计数 | `system.rs` 路由已注册；已增加共享 50ms probe 采样和字段契约 | 已补齐采样行为并通过鉴权/响应测试；GC 明示 Rust 不支持 |
| P1 | POST `/backtest/run` | 存在 Python 执行接口 | Rust 返回 501，历史 K 线 provider 和撮合未接入 | 应迁真实 backtest engine，避免伪造结果 |
| P1 | POST `/backtest/grid` | 存在 Python 执行接口 | Rust 校验参数后返回 501 | 与回测核心一并迁移，补参数和结果对照 |
| P1 | POST `/backtest/walkforward` | 存在 Python 执行接口 | Rust 校验参数后返回 501 | 与回测核心一并迁移，补切分/结果对照 |
| P1 | POST `/journal/label` | 存在 Python 决策回访实现 | Rust 返回 501，缺少实时价格注入/调度 | 迁移价格 provider、窗口、环境隔离后实现 |
| P1 | POST `/backup/create`、`/backup/upload`、POST restore、GET download、DELETE job | Python 提供备份操作 | Rust 明确返回 501；只有状态/调度读取接口 | 迁移文件打包、加密、校验、恢复回滚和权限审计 |

## 核实到的证据

- Python 探针：`backend/nanoquant/services/runtime_probe.py`；路由：`backend/nanoquant/api/routes/system.py`。
- Rust 探针与测试：`rust/nanoquant/src/system.rs`。
- Rust 回测 501：`rust/nanoquant/src/backtest.rs`。
- Rust journal 501：`rust/nanoquant/src/journal.rs`。
- Rust backup 501：`rust/nanoquant/src/backup.rs`。

后续扩展时逐个比对方法、路径、认证、成功/错误响应和副作用；完整差异必须覆盖所有 Python router decorators 和 Rust route registrations，完成前不可宣称 API 覆盖完整。
