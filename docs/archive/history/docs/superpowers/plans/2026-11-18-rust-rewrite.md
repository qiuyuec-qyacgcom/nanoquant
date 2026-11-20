> 历史记录：已退役，不可执行
> 源路径：docs/superpowers/plans/2026-11-18-rust-rewrite.md
> 归档时间：2026-11-20T09:17:25Z

# NanoQuant 全量 Rust 重写 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 subagent-driven-development（推荐）或 executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 用一个 Rust 进程完全替换 legacy 解释器入口，删除 Python 后端、测试与运维脚本，前端与 `/api/v1` JSON 契约保持不变。

**架构：** 新 crate `rust/nanoquant` 用 axum 提供 HTTP/WS，rusqlite 直接读写现有 SQLite 主库，把 `rust/exec-service` 的签名与下单逻辑吸收为内部模块。按域逐个替换路由，每域用契约测试钉住 Python 的响应形状后再切换入口。

**技术栈：** Rust 1.100（stable，x86_64-pc-windows-gnu）、axum 0.8、tokio、tower-http、rusqlite（bundled）、serde。

**规格：** 用户指令「该项目全面抛弃 python 全量使用 rust」。无独立设计文档；契约以 `backend/nanoquant` 现有行为为准，前端 `frontend/src/api` 为消费方。

## 全局约束

- 监听 `NANOQUANT_HOST`（默认 `0.0.0.0`）与 `NANOQUANT_PORT`（默认 `8899`），与 `backend/nanoquant/config.py` 一致。
- 所有 HTTP 路由保持前缀 `/api/v1`，响应字段名与 Python 版一致；未实现的路由返回 `501` 而不是 `404`，避免前端误判为“接口不存在”。
- 复用现有 SQLite 库 `data/nanoquant.db`，不新建表结构、不丢历史数据。
- 出厂密码拦截保持：`password_is_default=true` 时，除 `/auth/login`、`/auth/password`、`/auth/me`、`/health`、`/metrics` 与 `/public/` 外全部拒绝。
- 每个任务先写失败测试，再写实现，`cargo test -p nanoquant` 通过后才进入下一个。
- Python 代码在对应域的 Rust 实现与测试都落地前不删除。

## 审查重点（Review Focus）

- 未改出厂密码时访问交易接口必须返回 403，而不是放行或 500。
- 登录失败按来源 IP 计数，超过窗口上限返回 429，且不能把其它 IP 一起锁住。
- 改密后旧 token 立即失效（token_version +1），持旧 token 访问返回 401。
- 请求的路由已在 Python 存在但 Rust 尚未实现时返回 501 + 明确 detail，而不是空 200。
- SQLite 被占用或迁移版本不匹配时拒绝启动并给出路径，而不是建一个空库把历史数据旁路掉。

---

## 文件结构

- 创建：`rust/nanoquant/Cargo.toml` — 主服务 crate
- 创建：`rust/nanoquant/src/main.rs` — 入口、路由装配、健康检查
- 创建：`rust/nanoquant/src/auth.rs` — 登录、会话、改密（任务 2）
- 创建：`rust/nanoquant/src/db.rs` — SQLite 连接与启动校验（任务 3）
- 修改：`Dockerfile`、`docker-compose.yml`、`deploy/nanoquant.service`、`scripts/start.ps1` — 入口从 Python 换到 Rust 二进制（任务 7）
- 删除：`backend/`、`scripts/*.py` — 全部域迁移完成并验证后（任务 8）

### 任务 1：Rust 服务骨架与健康检查

**文件：**
- 创建：`rust/nanoquant/Cargo.toml`
- 创建：`rust/nanoquant/src/main.rs`
- 测试：`rust/nanoquant/src/main.rs` 内 `health_reports_rust_kernel`

- [ ] **步骤 1：编写失败的测试**

```rust
#[tokio::test]
async fn health_reports_rust_kernel() {
    let app = router(AppState { started_at: now() }, None);
    let response = app.oneshot(Request::builder().uri("/api/v1/health").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test -p nanoquant --manifest-path rust/nanoquant/Cargo.toml health_reports_rust_kernel`
预期：编译失败，`router` 未定义。

- [ ] **步骤 3：实现 `router`、`health`、`main`**

`GET /api/v1/health` 返回 `{"ok": true, "name": "NanoQuant", "version": <crate 版本>, "uptime_sec": <秒>, "kernel": "rust"}`。

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test -p nanoquant --manifest-path rust/nanoquant/Cargo.toml`
预期：PASS。

### 任务 2：鉴权（登录 / me / 改密 / 出厂密码拦截）

对齐 `backend/nanoquant/api/routes/auth.py` 与 `main.py` 的 `_PASSWORD_CHANGE_ALLOWED`。

- [ ] 失败测试：错误密码返回 401；同 IP 连续失败达到 `login_fail_limit` 返回 429；其它 IP 仍可登录。
- [ ] 失败测试：`password_is_default=true` 时 `GET /api/v1/engine/status` 返回 403，`GET /api/v1/auth/me` 返回 200。
- [ ] 失败测试：改密后旧 token 访问返回 401，新 token 返回 200。
- [ ] 实现 `auth.rs`，密码哈希算法与 Python `security.py` 的 `hash_password` 兼容，使现有库可直接登录。
- [ ] `cargo test -p nanoquant` 通过。

### 任务 3：SQLite 启动校验

- [ ] 失败测试：指向不存在的库路径时进程拒绝启动并在错误信息里包含该路径。
- [ ] 失败测试：库缺少 `app_settings` 表时拒绝启动。
- [ ] 实现 `db.rs`：打开 `NANOQUANT_DB`（默认 `data/nanoquant.db`），设置 WAL，不执行任何建表。

### 任务 4：只读域

按 Python 路由原样实现并各配一条契约测试：`symbols`、`strategies`、`journal`（只读端点）、`news`、`public`、`system` 的 `info`/`events`/`logs`。

### 任务 5：交易与引擎控制

迁移 `engine/worker.py`、`exchange/broker.py`、`exchange/client.py`，吸收 `rust/exec-service` 的签名与下单。覆盖：开停引擎、持仓、下单、平仓、保护单。每条对应 Python 测试里的一个失败用例改写为 Rust。

### 任务 6：AI、进化、回测、备份、资讯写入、WebSocket

迁移 `ai`、`evolution`、`backtest`、`backup`、`journal` 写入端、`signal_follow`，以及 `/api/v1/ws` 与 `/api/v1/ws/klines`。

### 任务 7：切换入口

- [ ] `Dockerfile`、`docker-compose.yml`、`deploy/nanoquant.service`、`scripts/start.ps1`、`scripts/start.sh` 的启动命令改为 `nanoquant` 二进制。
- [ ] CI 的 backend job 从 pytest 改为 `cargo test -p nanoquant`。
- [ ] 健康检查由内联解释器命令改为 `GET /api/v1/health`。

### 任务 8：删除 Python

- [ ] 确认任务 2-6 的契约测试全部通过。
- [ ] 删除 `backend/`、`scripts/*.py`，移除 `requirements*.txt`、`ruff.toml`、`pyproject` 相关配置与 dependabot 的 pip 条目。
- [ ] `cargo test --workspace` 与前端 `npm run build` 通过。
