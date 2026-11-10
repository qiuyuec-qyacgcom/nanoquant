<div align="center">

# NanoQuant

**小资金币安合约量化终端 · 最低 1 USDT 起 · 1–100x 杠杆 · AI 辅助决策 · 自进化心法**

<p>
  <img src="https://img.shields.io/badge/python-3.11%2B-3776AB?logo=python&logoColor=white" alt="Python">
  <img src="https://img.shields.io/badge/FastAPI-009688?logo=fastapi&logoColor=white" alt="FastAPI">
  <img src="https://img.shields.io/badge/Vue-3-4FC08D?logo=vuedotjs&logoColor=white" alt="Vue 3">
  <img src="https://img.shields.io/badge/TypeScript-3178C6?logo=typescript&logoColor=white" alt="TypeScript">
  <img src="https://img.shields.io/badge/SQLite-003B57?logo=sqlite&logoColor=white" alt="SQLite">
  <img src="https://img.shields.io/badge/Rust-sidecar-B7410E?logo=rust&logoColor=white" alt="Rust sidecar">
  <img src="https://img.shields.io/badge/mode-demo%20default-blue" alt="Demo default">
</p>

**一个面向极小资金的币安 U 本位永续合约量化系统：** 把行情、信号、风控、AI 复核与自进化复盘
串成一条**可解释、可审计**的执行链。默认模拟盘模式，所有参数热更新，支持选择性备份与还原。

[特性](#-特性) · [界面](#-界面) · [快速开始](#-快速开始) · [架构](#-架构) · [交易逻辑](#-交易逻辑) ·
[执行内核](#-执行内核-sidecar) · [参数](#-默认参数) · [API](#-api-概览) · [测试](#-测试) ·
[运维](#-运维与可观测) · [路线图](#-路线图) · [FAQ](#-faq)

</div>

---

> **近期更新（2026-11）**：主网演练手册与只读预检、systemd 部署、组合风险与波动率目标仓位、
> 心法效果对照与命中率回写、API 限频与 `/metrics`、轮询成本优化、持仓权威浮盈亏、
> 可用保证金截断与并发保证金账本（修复 `-2019`）、**紧凑决策协议 v2（省 ~50% 输入 / ~63% 输出，后台转中文）**、
> 下单精度按合约步进格式化（修复 `-1111`）、**模型思考强度可调（对齐 astra-quant-agent）**、
> **推理型模型库（14 族，含 GPT-6/o 系列/Gemini 3/Qwen3/Kimi K3/GLM-5 等）**、
> **持仓与账户浮盈亏同源同刻**、**K 线价格精度/静态价格/单根竞态修复**、
> **心法 AI 语义替代、旧版本自动删除与存量去重**。
> 逐项过程与实测数据见 [`docs/TASKS.md`](docs/TASKS.md) #56–#76。

## ✨ 特性

| 能力 | 说明 |
|---|---|
| 💸 **1 USDT 起跑** | 按交易所步进计算真实最小名义额，取整不足即明确拒绝；数量/价格严格按 `qty_step`/`price_tick` 定格小数位，避免 `-1111` |
| 📈 **1–100x 杠杆** | 单笔风险预算反推名义额，再被保证金上限与**可用余额**双重截断（留 10% 安全垫，避免 `-2019`）；杠杆只能逐层封顶 |
| 🤖 **AI 辅助开平仓** | OpenAI 兼容 / Anthropic 协议，单模型 / 多席位议会；输出强制 JSON 契约，**只能收紧风控、不能放宽**；小时/月度双重预算；默认紧凑英文协议 v2（后台转中文，省约 50% 输入 / 63% 输出）；**思考强度可调**（auto/none/minimal/low/medium/high/xhigh/max，Anthropic 映射 thinking 预算、OpenAI 映射 reasoning_effort），内置 **14 族推理模型库**自动识别支持范围 |
| 🧬 **自进化心法** | **每笔平仓由 AI 复盘**（低思考强度 + 足额输出预算，失败才回退规则），结论经防污染审计后沉淀为心法；语义重复时新版本**自动替代**并删除旧版本（AI 可点名 `replaces`，含 6h 冷却防抖动），存量重复可一键「合并重复」；按健康度回注后续决策，注入 ID 随成交落库，用「采用 vs 未采用」对照量化效果，胜负回写命中率 |
| 🧾 **信号审计** | 每一轮扫描的五因子评分、AI 复核与最终裁决；拒绝原因按 Top 分布，"为什么没开仓"一页看尽；相同状态的 flat 信号 60s 合并为一行并用 `repeats` 还原真实扫描次数 |
| 📰 **资讯与情绪** | 10 个源（含币安广场 / 公告），情绪指数与币种共现，可选注入 AI 上下文，源体检逐项给状态码 |
| 🌐 **前台 / 后台双端** | 前台公开站点（首页 / 观点 / 关于，响应式）+ 后台管理终端（需登录） |
| 📈 **前台 K 线（跟随标的池）** | 前台只展示后台已添加的合约，严格按添加顺序排列；空池显示「未添加标的」，增删 10 秒内自动同步 |
| 🎯 **astra 规则标的池** | 添加前用币安在线合约表校验（USDT 永续 / 交易中），分层派生杠杆与止损，去重 + 容量上限，删除需确认短语且持仓禁删 |
| 🧩 **执行内核 sidecar** | Rust 只签名不下单：`shadow` 旁路逐字节比对（零阻塞）、`enforce` 不一致直接拒单；掉线自动降级 + 5s 静默（详见[执行内核](#-执行内核-sidecar)） |
| 🧪 **回测与样本外验证** | 与实盘共用策略函数：计入手续费/滑点/点差/资金费，部分止盈同口径；参数网格 + walk-forward（前段选参、后段冻结）给出过拟合差距 |
| ⚖️ **组合风险控制** | 同向持仓上限（相关性代理）、波动率目标自动缩放仓位；下单按 `availableBalance` 截断，**同轮并发开仓共享一份保证金额度**（账本串行预留），避免两笔各自算满额后第二笔被 `-2019` 拒 |
| 📊 **权威盈亏口径** | 持仓浮盈亏直接取交易所 `positionRisk.unRealizedProfit`（按 markPrice 结算，与保护单一致）；账户接口抖动时沿用上次读数而不是刷成 0 |
| 🛡 **容错与可观测** | 写库单队列 + 死信补偿页、事件总线、9 项一键诊断、慢 SQL 日志、抖动探针、可选 IP 限频与 `/metrics` |
| 💾 **选择性备份** | 任意勾选配置组与数据文件，SHA-256 校验，还原前自动快照，主密钥不入包 |
| 🧰 **一键部署** | `install.sh` / `install.ps1` / `start.ps1` / `stop.ps1` / Docker Compose；另有 systemd unit（主服务 + sidecar，最小权限硬化） |
| 🩺 **主网演练手册** | [`docs/MAINNET_DRILL.md`](docs/MAINNET_DRILL.md) 给出三段演练判据与回滚；`scripts/mainnet_preflight.py --yes` 用实盘凭证做只读预检（不碰下单） |

---

## 🖥 界面

前台（公开，无需登录）与后台（`/admin`，需登录）视觉同源，移动端自动切换底部 Tab。

| 端 | 页面 | 内容 |
|---|---|---|
| 前台 | `/` | 状态条、标的池 K 线工作区、核心能力、心法、资讯 |
| 前台 | `/charts` | 行情终端：池内标的 K 线（按添加顺序）、周期切换、24H 概览 |
| 前台 | `/insights` | 资讯流、情绪指数、脱敏心法 |
| 前台 | `/about` | 开仓链路、技术栈、风险声明 |
| 后台 | `/admin/dashboard` | 权益曲线、账户、持仓（交易所权威浮盈亏 / ROI / 「缓存值」标记）、事件流、引擎控制、超短线执行 |
| 后台 | `/admin/onboarding` | 开仓检查：7 项前置条件 + 每标的最小名义额与所需保证金（按可用余额判定） |
| 后台 | `/admin/signals` | 信号审计：评分 / AI / 裁决 / 拒绝原因 Top / 24h 密度图；`×N` 标记合并的重复扫描，`summary.scans` 还原真实次数 |
| 后台 | `/admin/symbols` | 标的池（币安合约表校验、分层/精度/最小下单、删除确认短语） |
| 后台 | `/admin/chart` | K 线工作站：与前台同源组件，MA/EMA/BOLL + VOL/MACD/RSI，币安推送 + 收盘倒计时 |
| 后台 | `/admin/backtest` | 回测：净值曲线、分段统计、成交明细（含资金费/部分止盈）、参数网格、样本外验证 |
| 后台 | `/admin/news` | 资讯情绪 + 源检测 |
| 后台 | `/admin/ai` | 模型接入、诊断、**决策演练（原始英文输出 / 后台中文译文对照、token 估算）**、协议版本切换 |
| 后台 | `/admin/strategy` | 交易环境（模拟盘 / 实盘）+ 引擎/风控/策略/数据源/执行内核/**安全与观测**等分区参数 |
| 后台 | `/admin/evolution` | 心法库、注入预览、复盘记录、心法效果对照（4 张 KPI + 逐心法命中/胜率表） |
| 后台 | `/admin/backup` | 备份 / 还原 / 下载 |
| 后台 | `/admin/dead-letters` | 落库死信：查看失败载荷、重放、忽略 |
| 后台 | `/admin/system` | 系统诊断、执行内核状态、告警记录、归档、运行事件与日志 |

---

## 🚀 快速开始

### 方式一：一键脚本（推荐）

```bash
# Linux / macOS
bash scripts/install.sh     # 创建 venv、装依赖、构建前端、初始化数据库
bash scripts/start.sh       # 前台启动，默认 http://127.0.0.1:8899（--daemon 后台运行）
bash scripts/status.sh      # 查看状态
bash scripts/stop.sh        # 停止
```

```powershell
# Windows PowerShell
powershell -ExecutionPolicy Bypass -File scripts\install.ps1
powershell -File scripts\start.ps1
powershell -File scripts\stop.ps1        # 按命令行匹配停止，不误杀其它 Python 进程

# venv 不在仓库内时（如迁移后共用上级目录的解释器）
$env:NANOQUANT_VENV = 'F:\cb\.venvtest'
powershell -File scripts\start.ps1       # 也支持 -Python / -Port / -Debug
```

`start.ps1` 按 `-Python` → `NANOQUANT_VENV` → 仓库 `.venv` → 上级 `.venvtest` 顺序探测解释器，
迁移目录后不会因找不到虚拟环境而启动失败。

### 方式二：Docker

```bash
docker compose up -d
# 访问 http://127.0.0.1:8899；数据与日志挂载到 ./data、./logs
```

### 方式三：手动

```bash
python -m venv .venv && source .venv/bin/activate   # Windows: .venv\Scripts\activate
pip install -r backend/requirements.txt             # 锁定版本；源头是 backend/requirements.in
cd frontend && npm install && npm run build && cd ..

cd backend
python -m nanoquant        # 首次启动自动建库并创建 admin
```

首次登录：`admin / admin123`（登录后会被要求立即改密）。

> 生产守护（systemd）：主服务与 sidecar 的 unit 文件见 [`deploy/`](deploy/README.md)，含最小权限硬化与 UDS `0600` 权限设置。
> 上主网前请先读 [`docs/MAINNET_DRILL.md`](docs/MAINNET_DRILL.md)（三段演练、判据、回滚），并跑一次
> `python scripts/mainnet_preflight.py --yes` 只读预检。

### 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `NANOQUANT_HOME` | 仓库根目录 | 运行目录基准；迁移时**整个 data/ 必须一起搬**（含 `.master_key`） |
| `NANOQUANT_PORT` / `NANOQUANT_HOST` | `8899` / `0.0.0.0` | 监听地址 |
| `NANOQUANT_DATA_DIR` / `NANOQUANT_DB` | `$HOME/data`、`data/nanoquant.db` | 数据目录与库文件 |
| `NANOQUANT_LOG_DIR` | `$HOME/logs` | 日志目录 |
| `NANOQUANT_KEY_FILE` | `data/.master_key` | 主密钥；**丢失后已加密的 API 密钥无法解密** |
| `NANOQUANT_FRONTEND` | `$HOME/frontend/dist` | 前端产物目录 |
| `NANOQUANT_VENV` | 空 | Windows 启动脚本的虚拟环境目录（迁移场景） |
| `EXEC_SOCKET` | 空 → `/run/nanoquant/exec.sock` | sidecar 地址，如 `unix:/run/nanoquant/exec.sock` / `tcp:127.0.0.1:9101`；参数页 `exec_socket` 优先 |
| `EXEC_SECRET` | 无 | sidecar 使用的签名 Secret（生产放 `EnvironmentFile`，权限 `0600`） |
| `EXEC_TCP` | 空 | Windows / 容器联调用的回环 TCP 地址，如 `127.0.0.1:9101` |
| `E2E_BASE_URL` / `E2E_CHANNEL` | `http://127.0.0.1:8899` / 空 | Playwright 冒烟目标与浏览器通道（如 `msedge`） |
| `NANOQUANT_DEBUG` / `NANOQUANT_SALT` | `false` / 内置值 | 调试开关 / 口令盐 |

> 端口与路径只在这里读一次；其余业务参数全部存在库里，后台改完即生效（≤1.5s 缓存）。

---

## 🧭 架构

```
nanoquant/
├── backend/nanoquant/
│   ├── engine/          # 交易引擎：主循环、信号、风控、出场规则、指标
│   ├── exchange/        # 币安行情 + 签名客户端（exec_client：sidecar 长连接）、执行层、精度与限频
│   ├── ai/              # 多模型调用、JSON 契约决策、议会聚合、诊断
│   ├── evolution/       # 复盘、心法库、防污染审计、effectiveness（采用/未采用对照）
│   ├── news/            # 资讯源适配、情绪打分、聚合与源体检
│   ├── backtest/        # 回测引擎 + 参数网格 + walkforward（与实盘共用策略函数）
│   ├── services/        # 配置热更新、运行态、事件总线、标的池、备份、写库队列、api_guard、
│   │                    # exec_shadow、position_view（权威盈亏口径）、诊断
│   ├── api/routes/      # auth / system / symbols / trade / backtest / ai / evolution / news / backup / public / ws
│   └── main.py          # FastAPI 装配 + SPA 托管
├── frontend/src/
│   ├── views/site/      # 公开前台（首页 / 观点 / 关于）
│   ├── views/           # 后台页面
│   ├── components/      # EChart / EnvironmentSwitch / Modal / EventFeed ...
│   └── layouts/         # SiteLayout（前台）/ ShellLayout（后台）
├── frontend/e2e/        # Playwright 冒烟（首页 / 登录 / 4 个后台关键面板）
├── rust/exec-service/   # 执行内核 sidecar（P2：只签名不下单，UDS + JSON Lines）
├── deploy/              # systemd unit（主服务 + sidecar）与部署说明
├── scripts/             # install / start / stop / status + 压测/对账/校验/主网预检工具
├── backend/tests/       # 契约 + 路由测试（临时目录隔离）/ smoke / evolution / ai / news 探测
├── backend/requirements.in / requirements.txt   # 直接依赖 → 锁定版本
├── docs/                # RESEARCH 调研 / TASKS 任务书 / EXEC_PROTOCOL 协议 / MAINNET_DRILL 演练手册
├── .github/workflows/   # CI：pytest / typecheck+build / cargo test / Playwright e2e
├── Dockerfile
└── docker-compose.yml
```

**数据层**：SQLite（WAL）+ SQLAlchemy 2.x。所有运行态（冷却锁、连亏计数、日切锚点、阶段耗时）存
`data/engine_state`，重启不丢；事件、信号、成交、心法全部入库可追溯。

**写入模型**：业务线程只投递，单写线程串行落库（避免 `database is locked`）；持续失败进死信表，
后台可重放，窗口外的过期数据先 gzip 归档再删除。

---

## ⚙ 交易逻辑

引擎每 15 秒一轮（下限 15s，可配置），七步链路，**任何一步不通过都不开仓，原因写入信号审计**：

```
① 取价      引擎内部直接调币安公共接口（5–30m K 线 + 资金费率 + 持仓量），无本地行情模块
            K 线 20s 缓存、资金费率/持仓量 60s 缓存，ticker/前台 K 线走 WS 不占 REST 权重
② 信号      五因子加权得分 ∈ [-1,1]
            趋势0.32 / 动量0.26 / 量能0.18 / 资金费率0.12 / 持仓量0.12
            准入：ATR%∈[0.05,4]、量比≥0.80、24h成交额≥300万
            结构确认：多头需 EMA9>EMA21，空头反之（避免逆势抄底）
③ 风控      全局：权益≥0.5U、日内亏损≤25%、回撤≤50%、连亏<4笔、无熔断
            单标的：未持仓、持仓数<2、无冷却、总敞口<2000%
            组合：同向持仓上限（相关性代理）、波动率目标自动缩放仓位
④ 仓位      风险预算 = 权益×10% × 波动率缩放系数(0.25–2x)
            名义额 = min( 风险预算 ÷ 止损%, 权益×50%×杠杆, 可用余额×0.9×杠杆 )
            数量 = 名义额 ÷ 价，按步进向下取整；低于最小可下单量 / 可用保证金不足 → 明确拒绝
⑤ AI 复核  只能「同意 / 否决 / 降杠杆 / 收窄止损」，不能放宽任何风控
            解析失败、超时、调用失败 → fail-closed 跳过
⑥ 下单      市价 + 止损/止盈保护单（模拟盘走币安测试网，实盘走主网；与数量解耦）
⑦ 持仓管理  硬止损 / 止盈 / 阶梯止盈 / 移动止损 / 超时 / 信号反转
            浮盈 R≥1.0 先部分止盈（默认 50%），剩余仓位止损上移保本
```

平仓后自动进入复盘：规则兜底 + AI 复盘 → 防污染审计 → 心法入库（带健康度、版本、替代关系）
→ 每 N 笔触发全局复盘 → 心法回注下一次 AI 决策。

---

## 🧩 执行内核 sidecar

把"签名"这件最不能出错的事交给 Rust：Python 算完签名后另送一份给 sidecar，
**逐字节比对**成功才继续。sidecar 只持有 Secret 并回传签名，不接触账户、持仓与订单。

| 模式 | 关键路径开销 | 行为 |
|---|---|---|
| `off`（默认） | 无 | 只用 Python 本地签名 |
| `shadow` | 后台任务，**零阻塞** | 每次签名旁路比对，只统计；抽样比例可调 |
| `enforce` | 同步 +0.5–1ms | 不一致**立即拒绝下单**（转 `-1022`） |

- **只有不一致才 fail-closed**：sidecar 连不上/超时一律降级回 Python 签名，并进入 5s 静默，
  不会让每一单都等一次超时；
- 比对延迟口径是 **sidecar 真实往返**（锁内计时），不含排队；掉线时最大延迟≈超时值；
- 不一致会写入事件总线（error 级 → 值班告警），并在系统页留 `local/remote/echo` 三份摘要；
- 切流判据是 `mismatched == 0` 且自检通过，**不是"连上了"**。

配置：参数页「系统 → 执行内核（sidecar）」——`exec_mode` / `exec_socket` / `exec_sample_pct` / `exec_timeout_ms`。
协议细节与时间线见 [`docs/EXEC_PROTOCOL.md`](docs/EXEC_PROTOCOL.md)，当前进度见 [`docs/TASKS.md`](docs/TASKS.md)。

> P2 已完成并**已编译验证**：Rust 二进制（release 0.91MB）在真实测试网 Ed25519 凭证下与 Python 逐字节一致，
> `shadow` p50 0.50ms / p99 1.18ms，`enforce` p50 0.18ms / p99 0.40ms；错误密钥会被判为不一致、进程被杀 3s 内降级。
> 尚未覆盖：Linux UDS 部署、systemd 守护、`enforce` 24h 灰度；P3（接管下单）未开始。构建与验证命令见
> [`rust/README.md`](rust/README.md)。

---

## 🔑 默认参数

| 组 | 参数 | 默认 |
|---|---|---|
| 引擎 | 周期（下限 15s） / 同时持仓 / 每轮扫描 / 时段 / 交易环境 | 15s / 2 / 12 / 全天 / demo 模拟盘 |
| 风控 | 单笔风险 / 默认杠杆 / 保证金占比 / 总敞口 | 10% / 20x / 50% / 2000% |
| 风控 | 日内亏损 / 回撤 / 连亏上限 / 冷静期 | 25% / 50% / 4 笔 / 10 分钟 |
| 组合 | 同向持仓上限 / 波动率目标 | 0 不限制 / 0 关闭（建议小资金设 1 / 1.5） |
| 资金 | 可用保证金安全垫 | 0.9（下单按 `availableBalance` 截断，防 `-2019`） |
| 策略 | 周期 / 阈值 / 量比 / ATR% / 最长持仓 | 5m / 0.30 / 0.80 / 0.05–4 / 180 分钟 |
| 执行 | 盘口点差闸门 / 时间阶梯止盈 | 8 bps / 0min 0.8%、15min 0.6%、30min 0.45%、60min 0.3%、120min 0.2% |
| 执行内核 | 模式 / 抽样 / 超时 | off / 100% / 500ms |
| 安全 | API 限频 / Prometheus 指标 | 0 关闭（按 IP 计次） / 开启 |
| 轮询 | 资金费率缓存 / 持仓量缓存 / flat 信号合并窗口 | 60s / 60s / 60s（`repeats` 计数） |
| 心法 | 效果对照最小样本 | 每组 ≥ 10 笔才给结论（观察性统计） |
| AI | 模式 / 最低置信 / 调用上限 / 月预算 | single / 0.45 / 30 次每小时 / $3 |
| AI | 决策协议 / 供应商 max_tokens | v2 紧凑英文 + 后台转中文 / 3000（推理型模型防截断） |
| AI | 思考强度 | `auto`（跟随服务端默认）；可选 none/minimal/low/medium/high/xhigh/max |

> 以上均为**出厂默认值**，可在「参数配置」热更新；例如当前运行环境的 AI 配额、杠杆与风险参数可能已被调整。

> 1 USDT 买不了 BTC：BTCUSDT 最小名义额以币安合约表为准（当前约 50–100 USDT）。小资金请选 DOGE / SOL / ETH 等小面值标的，
> 或提高保证金比例 / 杠杆。面板会在「开仓检查」和信号审计里给出这一原因。

---

## 🔌 API 概览

统一前缀 `/api/v1`，后台接口需 `Authorization: Bearer <token>`；`/api/v1/public/*` 为公开只读。

| 分组 | 端点 |
|---|---|
| 认证 | `POST /auth/login`、`GET /auth/me`、`POST /auth/password` |
| 标的池 | `GET /symbols`、`GET /symbols/available`、`POST /symbols`、`POST /symbols/bulk`、`PATCH /symbols/{symbol}`、`DELETE /symbols/{symbol}?confirmation=REMOVE%20<SYMBOL>` |
| 交易 | `GET /engine/status`、`POST /engine/control`、`POST /engine/cycle-now`、`GET /engine/admission`、`/engine/positions`、`POST /engine/positions/{id}/close`、`/engine/orders`、`/engine/protection`、`/engine/execution`、`/engine/quotes`、`/engine/signals`、`/engine/trades`、`/engine/equity`、`/engine/stats`、`GET /chart/klines` |
| 回测 | `POST /backtest/run`、`POST /backtest/grid`、`POST /backtest/walkforward`（样本外验证） |
| AI | `GET/POST /ai/providers`、`PATCH/DELETE /ai/providers/{id}`、`POST /ai/providers/fetch-models`、`POST /ai/providers/{id}/test`、`GET /ai/providers/{id}/diagnose`、`GET /ai/providers/{id}/models`、`POST /ai/analyze`、`POST /ai/decision-preview`、`GET /ai/logs`、`GET /ai/usage` |
| 自进化 | `GET/POST /evolution/mantras`、`PATCH/DELETE /evolution/mantras/{id}`、`GET /evolution/injection`、`GET /evolution/runs`、`POST /evolution/review/{trade_id}`、`POST /evolution/global-review`、`GET /evolution/stats`、`GET /evolution/effectiveness` |
| 资讯 | `GET /news`、`POST /news/refresh`、`GET /news/overview`、`GET /news/diagnostics`、`POST /news/ai-summary` |
| 备份 | `GET /backup/options`、`POST /backup/create`、`GET /backup/jobs`、`POST /backup/{id}/restore`、`DELETE /backup/{id}`、`GET /backup/{id}/download` |
| 配置 | `GET /settings`、`GET/PUT /settings/{group}`、`POST /settings/{group}/reset`、`GET/POST/DELETE /settings/credentials/exchange` |
| 系统 | `GET /health`、`GET /system/info`、`GET /system/diagnostics`、`GET /system/events`、`GET /system/logs`、`GET /system/runtime`、`GET /system/write-health`、`GET /system/system-settings`、`GET /system/archives`、`GET /system/archives/{name}`、`GET /system/dead-letters`、`POST /system/dead-letters/{id}/replay`、`POST /system/dead-letters/{id}/ignore`、`POST /system/dead-letters/replay-all`、`GET /system/alerts/logs`、`POST /system/alerts/test` |
| 执行内核 | `GET /system/exec-sidecar`、`POST /system/exec-sidecar/self-test`、`POST /system/exec-sidecar/reset` |
| 指标 | `GET /metrics`（Prometheus 文本，只读免鉴权；仅队列/死信/比对计数与状态位） |
| 公开 | `GET /public/config`、`/public/overview`、`/public/pool`（按添加顺序）、`/public/quotes`、`/public/klines`、`/public/klines/batch`（仅池内合约）、`/public/news`、`/public/mantras`、`/public/performance` |
| 实时 | `WS /ws?token=...`（事件流）、`WS /ws/klines?symbol=&interval=`（K 线推送，服务端复用币安 WebSocket 流，不占 REST 权重） |

公开接口严格脱敏：不含账户余额、持仓、订单、密钥与心法正文；绩效默认关闭，开启后也只给比例。

---

## 🧪 测试

```bash
cd backend
python -m pytest tests -q         # 契约 70 + 路由 17（不联网、不碰真实 data/）
python tests/smoke_test.py        # 端到端：登录→标的池→引擎→信号→备份→SPA
python tests/evolution_test.py    # 自进化闭环：平仓→复盘→心法→去重→防污染
python tests/ai_test.py           # AI 链路：诊断/测试/演练/议会（需本地 Mock 模型）
python tests/mock_llm.py          # 启动本地 Mock OpenAI 兼容服务
python tests/news_probe.py        # 资讯源体检
```

契约测试覆盖：部分止盈 R 计算、阶梯止盈档位、出场优先级、配置校验、分位耗时、回测与参数网格、
资金费结算与多付空收、部分止盈记账与保本止损、点差成本、walk-forward 结构、
单行 PEM 归一化、Ed25519/HMAC 签名、告警静默、写库重试分类、sidecar 协议与影子判定、
心法效果分组统计、组合风险与波动率缩放、15s 轮询下限与老库迁移、轮询缓存与信号合并、
可用保证金截断与 `-2019` 文案、持仓浮盈亏快照优先与外部仓位写入、
紧凑协议 v2 的 token 裁剪 / v1-v2 自动探测 / 枚举码中文转译 / 截断归因、
下单数量与价格的步进精度格式化与 `-1111` 文案、心法语义替代 / 冷却 / AI 点名替代 / 存量去重 /
复盘后自动删除旧版本。
路由测试覆盖鉴权、配置校验、回测/信号汇总与只读观测接口，运行在临时目录（`tests/conftest.py` 强制隔离）。

签名链路与性能工具（需在仓库根目录执行）：

```bash
python scripts/verify_sidecar.py                                        # 自起 mock，协议 + 签名比对
EXEC_SECRET=... python scripts/verify_sidecar.py --target unix:/run/nanoquant/exec.sock  # 真实 sidecar 逐字节复核
python scripts/verify_sidecar.py --target tcp:127.0.0.1:9101            # Windows 回环 TCP（不传 Secret 只验协议）
python scripts/mock_exec_sidecar.py --port 9101 --secret "$SECRET"      # 本地 mock sidecar（联调）
python scripts/bench_exec.py --orders 200 --delay-ms 60                 # 分阶段耗时压测
python scripts/p99_real.py --orders 15 --per-minute 5                   # 真实网络 p99 采样
python scripts/rtt_probe.py --target testnet --seconds 60               # 交易所 RTT 采样
python scripts/reconcile.py --self-test                                 # 回测/实盘对账（或 --backtest/--live 传 CSV）
python scripts/verify_partial_tp.py                                     # 部分止盈边界复核
python scripts/mainnet_preflight.py --yes                               # 主网演练前只读预检
```

前端 e2e 冒烟（需先启动服务；Windows 用系统 Edge 免下载浏览器）：

```powershell
cd frontend
$env:E2E_CHANNEL = 'msedge'
npm run e2e       # 首页 / 登录 / 4 个后台关键面板，路由是 hash 模式
```

> CI：`.github/workflows/ci.yml` 在 push / PR 时跑 `compileall + pytest tests`（后端）、
> `npm ci + typecheck + build`（前端）、`cargo test --release`（Rust）与
> 临时库起服务的 Playwright 冒烟（chromium）。
> 依赖版本由 `backend/requirements.txt` 锁定，源头是同目录的 `requirements.in`。

---

## 🛠 运维与可观测

- **一键诊断（9 项）**：数据库 / 写库队列 / 落库补偿 / 交易所接入 / 交易通道 / 引擎心跳 / AI 模型 / 存储 / 执行内核；
  每项给结论与耗时，异常项直接指向下一步。
- **事件总线**：warn/error 落库 + 实时推送（WS），可配置 Webhook / Telegram、静默时段、摘要合并、冷却去重。
- **运行态探针**：`/system/runtime` 给出事件循环 lag 分位与 GC 暂停峰值/累计，用来区分"网络慢"还是"Python 自己卡"。
- **死信补偿**：写库失败进 `dead_letters`，后台重放或忽略；`/system/write-health` 给出积压与最近耗时。
- **归档**：过期事件/信号先 gzip 到 `data/archive/` 再批量删除，可在系统页预览。
- **备份**：选择性打包 + SHA-256 校验，还原前自动快照；主密钥不入包（换机需另行迁移 `data/.master_key`）。
- **限频与指标**：`system.api_rate_limit_per_min` 按 IP 限流（0=关闭，健康检查与 `/metrics` 豁免）；
  `GET /metrics` 提供 Prometheus 文本（引擎状态、写队列、死信、sidecar 比对计数），不含任何账户或密钥数据。
- **轮询成本**：资金费率/持仓量按 60s 缓存；相同状态的 flat 信号 60s 内合并为一行并以 `repeats` 计数，
  审计页与密度图按 `repeats` 还原真实扫描次数；引擎轮询下限 15s（5m 策略无需更快）。
- **盈亏口径**：持仓浮盈亏优先取交易所 `positionRisk` 的 `unRealizedProfit`（按 markPrice 结算，
  与保护单一致）；`/engine/status` 会把 `account()` 与 `positions()` **并行拉取**并在同一响应内合并，
  因此「当前持仓」逐行盈亏与顶部「浮动盈亏」同源同刻（实测差值 0.0）。实时仓位读不到时才回退本地估算
  并显示「估算」标签；账户接口抖动时沿用上次读数而不是刷成 0。
- **保证金校验**：下单数量按 `availableBalance`（而非总权益）截断并留 10% 安全垫，避免已有仓位
  占用保证金后仍算出买不起的数量、被交易所以 `-2019 Margin is insufficient` 拒绝。
- **组合风险**：同向持仓上限（默认关闭，建议小资金设 1）、ATR 波动率目标缩放（0.25–2x），
  在「参数配置 → 风控 → 仓位与杠杆」调整。
- **心法效果**：自进化页的「心法效果对照」给出采用组/对照组的笔数、胜率、单笔盈亏差与逐心法命中率；
  样本不足（任一组 < 10 笔）时只显示数据不下结论，且明确标注为观察性对照而非随机 A/B。
- **心法替代**：新复盘产出的心法与旧条目语义重复时自动替代（标题+规则加权相似度 ≥ 0.48，
  6 小时冷却防抖动；AI 可在 `replaces` 里点名旧标题，跨分类也生效）。
  被替代的旧版本在**复盘结束后自动物理删除**（`purge_superseded()`，幂等），列表里不会堆积置灰条目；
  手动「停用」的心法（`superseded_by=0`）不受影响。存量重复用「分析心法库 → 合并重复」一键整理
  （AI 分批判断，失败回退字符相似度），下一次复盘时旧版本同样会被清掉。
- **部署守护**：`deploy/nanoquant.service` 与 `deploy/nanoquant-exec.service` 提供 systemd 开机自启、
  崩溃拉起、最小权限硬化；`deploy/README.md` 内含 Prometheus 抓取配置与建议告警规则。
- **AI token 与审计**：决策默认走紧凑协议 v2（英文短键进、动作码/枚举码出，后台按码表转中文），
  输入约省 50% / 输出约省 63%；模型原文存 `AiCallLog.raw_text`，`/ai/logs` 与「决策演练」页可对照查看。
  `finish_reason=length` 或只有 thinking 输出时会明确报「输出被 max_tokens 截断」，不再笼统报解析失败。
- **思考强度**：每个供应商可单独设置 auto/none/minimal/low/medium/high/xhigh/max；
  Anthropic 走 `thinking.budget_tokens`（low 2k → max 64k，`max_tokens` 自动调大），
  OpenAI 兼容走 `reasoning_effort`（只对推理型模型注入）。网关不支持该参数时会自动移除并重试一次，
  不会导致整次调用失败。
- **推理模型库**：`ai/reasoning.py` 维护 14 个模型族（GPT-6/5、o 系列、GPT-OSS、Claude 3.7+/4、
  Gemini 2.5/3、DeepSeek V4/R1、Qwen3/QwQ、Kimi K2.7/K3、GLM-5、MiniMax M2、MiMo V2、Grok），
  另有一张负向表避免 `gpt-4o/qwen2.5/llama/-chat` 被误判。`GET /ai/reasoning-models?model=` 返回
  匹配结果与逐档可用性；xhigh/max 仅旗舰推理模型可用，选了会自动回退。

---

## 🗺 路线图

已完成（细节见 `docs/TASKS.md`）：

- [x] 回测引擎 + 参数网格（与实盘共用出场规则）
- [x] 回测保真度：资金费 / 部分止盈 / 点差成本 + walk-forward 样本外验证
- [x] WebSocket K 线推送（服务端复用币安流，不占 REST 权重）
- [x] 执行内核 P2：UDS 协议 + 影子签名比对 + `enforce` 闸门 + 面板自检（已编译验证）
- [x] 写库单队列 + 死信补偿 + 事件循环抖动探针
- [x] 路由层测试 + 依赖锁定 + CI 覆盖 Rust / 前端 e2e
- [x] 心法注入快照与「采用 vs 未采用」效果对照（观察性统计）+ 命中率回写
- [x] 主网演练手册与只读预检工具；systemd 部署 unit 与 Windows 启停脚本
- [x] 组合风险控制：同向持仓上限 + 波动率目标仓位
- [x] API 限频 + Prometheus `/metrics` 指标
- [x] 轮询成本优化：资金费率缓存 / flat 信号合并 / 15s 下限
- [x] 持仓浮盈亏改用交易所权威口径 + 账户读数缓存兜底
- [x] 下单按可用保证金截断，修复 `-2019 Margin is insufficient`
- [x] 紧凑决策协议 v2：token 裁剪 + 后台中文转译 + 截断归因 + 原文审计 + 决策演练对照
- [x] 下单数量/价格按合约步进格式化，修复 `-1111 Precision is over the maximum`
- [x] 同轮并发开仓共享保证金额度（账本串行预留），修复第二笔 `-2019`
- [x] 模型思考强度（reasoning effort）按供应商可调，对齐 astra-quant-agent 的档位与协议映射
- [x] 推理型模型库（14 族 + 负向表）与 `/ai/reasoning-models` 查询端点
- [x] 总览持仓浮盈亏与顶部 KPI 同源同刻（`account` + `positions` 并行取数合并）
- [x] K 线静态价格检测与空数据占位（模拟盘无成交标的不再画成直线/0）
- [x] K 线首帧竞态修复：WS 先到只缓存，历史到达后合并，单根不再渲染成点
- [x] K 线价格精度自适应（2–8 位小数）：ONE/JCT 等小价格币不再显示成 0.00 / 直线
- [x] 心法 AI 逐笔复盘 + 语义替代（`replaces`/0.48 阈值/6h 冷却）+ 存量 AI 去重
- [x] 复盘结束后自动物理删除被替代的旧心法（幂等，手动停用不受影响）

下一步：

- [ ] **主网小额演练**：按 [`docs/MAINNET_DRILL.md`](docs/MAINNET_DRILL.md) 跑开平 / 部分止盈 / 资金费三段
- [ ] 执行内核 P3：Linux UDS 部署 + systemd，接管下单与保护单
- [ ] 回测 / 实盘同口径对账（积累 30 笔实盘样本后校准滑点与资金费假设）
- [ ] 多账户 / 子账户隔离
- [ ] 更多数据源（OKX / Bybit 合约）
- [ ] 心法可视化时序（健康度演变）

---

## ❓ FAQ

**Q：引擎启动了但没有交易？**
A：按优先级检查「开仓检查」页——标的池是否为空（当前设计：空池不交易）、是否在交易时段、
是否触发连亏冷静期、标的可分配名义额是否够最小下单量。

**Q：为什么加不进标的 / 引擎不开仓？**
A：系统没有任何本地演示行情，价格与合约规格都必须来自币安：
1. 添加标的会用币安在线合约表校验（需能访问所选环境的域名），失败时接口会明确返回原因；
2. 模拟盘与实盘各有独立 API 凭证，未配置当前环境的凭证时无法启动引擎；
3. 引擎每轮直接读币安公共接口，网络不可达时这一轮直接跳过（不会用假数据下单）。
在「参数配置 → 数据源」填写 HTTP 代理，或切换网络后到「系统诊断」重新探测。

**Q：资讯源抓不到？**
A：币安广场 / 公告 / CoinDesk 需要能访问对应域名。资讯页的「源检测」会逐个给出状态码、
耗时与建议；也可在配置里填写资讯专用代理或自建 RSSHub。

**Q：信号审计里出现 `-2019 Margin is insufficient`？**
A：说明已有仓位/挂单占用了可用保证金，而这一单按总权益算出的数量实际上付不起初始保证金。
现在系统会在下单前按 `availableBalance` 截断数量并留 10% 安全垫；**同一轮多个标的并发开仓时
共享一份额度（保证金账本串行预留）**，后一笔只能用前一笔剩下的保证金，不够就明确拒绝，
不会两笔各自算满额再被交易所打回。若仍遇到（例如下单瞬间价格跳变导致保证金略超），
可平掉部分仓位、降低「单仓保证金 / 单笔风险」或适当提高杠杆。

**Q：信号审计里出现 `-1111 Precision is over the maximum defined for this asset`？**
A：说明发给交易所的数量/价格小数位超过了该合约允许的精度（每个标的的 `qty_step` / `price_tick` 不同）。
曾经用 `:.12f` 格式化数量，`12417.2` 会带出 `12417.200000000001` 这种二进制浮点尾，
在只允许 1 位小数的合约上被拒。现在所有下单参数（主单、平仓、保护单、移动止损）
都按标的池里的步进定格小数位；若仍遇到，检查该标的的 `precision/tick_sz` 是否与交易所一致，
删除后重新添加即可刷新规格。

**Q：持仓行的浮盈亏和「浮动盈亏」KPI 现在一致了吗？**
A：一致。`/engine/status` 会用 `asyncio.gather` **并行**拉取 `account` 与 `positions`，
把同一次请求里的 `positionRisk.unRealizedProfit` 合并进每一行持仓，两个面板同源同刻（实测差值 0.0）。
若仍看到差异，通常只有两种情况：① 账户接口本轮失败，KPI 沿用上次读数并标记「缓存值」；
② 交易所有未记录到本系统的外部仓位（逐行合计自然小于账户总额）。实时仓位读不到时，
持仓行会回退本地估算并显示「估算」标签。

**Q：为什么某些币的 K 线是一条直线，甚至价格显示为 0？**
A：按概率排查两种原因：

① **价格精度（最常见）**：`ONE`（0.001485）、`JCT`（0.009523）这类小价格币，如果图表按 2 位小数
渲染就会被舍入成 `0.00 / 0.01`，坐标轴按 0.01 步长展开，所有蜡烛挤在同一水平线上。
现在会按最近价格自动设置 2–8 位精度（ONE 5 位、BTC 2 位），tooltip 与坐标轴都能正确显示。

② **模拟盘没有真实成交**：测试网有 679 个永续、570 个状态正常，但部分合约 24h 成交额只有
0.0x USDT（如 `SXPUSDT`），K 线是固定价 + 0 成交量；还有 92 个合约连 24h ticker 都没有。
现在连续 `high==low` 会提示「静态价格」，空数据显示「暂无 K 线数据」占位；引擎的 ATR/量比闸门
也会自动跳过它们，不会开仓。

如果交易所数据正常但图表只显示一根（或切换后短暂 0 轴），那是历史 K 线与 WS 首帧的竞态：
首帧现在会先缓存、等历史到达后合并，单根也不再渲染成点。仍异常请 `Ctrl+F5` 强刷缓存。

**Q：信号审计里的信号行数怎么变少了？**
A：同一标的、方向、拒绝原因都相同的 flat 信号会在 60 秒内合并成一行，用 `×N` 标记并累计 `repeats`。
KPI「扫描信号」显示的是按 `repeats` 还原的**真实扫描次数**，按小时密度图与拒绝原因 Top 分布同样按权重统计，
所以统计口径没有变，只是不再每轮都写一行（写入量降约 88%）。

**Q：AI 模型输出怎么是一串英文短 JSON？面板里却是中文？**
A：默认启用**紧凑协议 v2**：英文短键进、动作码 + 枚举码出（约省 50% 输入 / 63% 输出），
后台按码表把 reason / 风险点 / 证伪条件转译成中文再落库，所以面板、信号审计、心法复盘看到的都是中文。
模型原文保存在 `AiCallLog.raw_text`，可在「AI 模型 → 决策演练」或 `/ai/logs` 对照查看。
若自定义模型只会回 v1 的详细中文 JSON，解析时会自动探测，无需改配置；也可在 AI 页把协议切回 v1。

**Q：为什么提示「AI 不可用（无法从模型输出中解析出 JSON 对象）」？**
A：说明模型返回里没有完整 JSON，系统按 **fail-closed 跳过这一笔**（绝不猜方向）。常见三种：
① 推理型模型把输出预算花在 thinking 上，`finish_reason=length` 导致 JSON 被截断 → 把供应商
`max_tokens` 提到 3000+ 或换非推理模型；② 模型只回了 reasoning 没回正文；③ 输出夹了散文/未闭合括号。
现在前两种会直接报「输出被 max_tokens 截断」/「只返回 thinking」，可在「AI 模型 → 决策演练」复现并查看原文。

**Q：模型设置里的「思考强度」怎么选？**
A：`auto` 交给服务端默认，`none` 明确关闭思考，其余档位按推理预算递增（minimal < low < medium < high < xhigh < max）。
Anthropic 协议会转成 `thinking.budget_tokens`（low 2k / medium 8k / high 16k / xhigh 32k / max 64k，
`max_tokens` 自动调大以保证大于预算）；OpenAI 兼容协议转成 `reasoning_effort`，只对推理型模型注入。
强度越高越慢越贵，但决策质量未必线性提升；建议先用「决策演练」对比 20 次再定。
网关不支持该参数时会自动移除并重试一次，不会导致调用失败。
支持哪些模型可在表单下方展开「支持的推理模型族」，或直接请求 `GET /ai/reasoning-models?model=你的模型`。

**Q：为什么有些模型没有注入 `reasoning_effort`？**
A：内置模型库只对识别为推理型的模型注入（14 个族），普通模型（gpt-4o / qwen2.5 / llama / `-chat` 等）
传了会被服务端 400，所以默认不发。Anthropic 协议不受此限制——`thinking.budget_tokens` 按档位直接生效。
模型不在库里但确实支持该参数时，可把档位设为 `auto` 之外的任意值并观察日志：服务端接受就说明直连可用，
返回 400/422 时系统会自动移除参数重试。

**Q：心法库会越积越多、互相矛盾吗？**
A：不会。新复盘产出的心法会先与全部生效心法比对：语义重复且更优就**替代**旧版本，
相似但不重复则合并强化。AI 还可以在 `replaces` 里直接点名旧标题，跨分类也能替代。
为防止同一晚反复翻烧饼，刚入库 6 小时且一次未被验证的心法只合并、不替代。
**被替代的旧版本会在本轮复盘结束时自动删除**（`purge_superseded()`，幂等），
所以心法列表里不会出现大量「被替代」的置灰条目；手动停用的心法不会被删。
修复前积累的重复条目，可在「分析心法库」点「合并重复」，由 AI 分批判断（失败回退字符相似度），
下一次复盘时旧版本同样会被清掉。

**Q：为什么有些复盘显示「AI 复盘失败, 使用规则兜底」？**
A：推理型模型对复盘任务平均要 2.5k~4k thinking tokens，供应商 `max_tokens` 太小时会被截断。
系统已把复盘调用的思考强度压到 `low` 并单独给 8000 输出预算、去掉 K 线明细与冗余规则，
正常应走 AI；若网关仍不支持这些参数，会回退到确定性的规则心法，不会中断自进化。

**Q：心法会越来越不可控吗？**
A：不会。心法入库前经防污染审计（拦截提高杠杆、取消止损等危险结论），并带健康度与版本；
全局复盘会淘汰低分心法，新心法以"替代"关系覆盖旧结论。自进化页另有「采用 vs 未采用」对照，
但那是观察性统计（非随机 A/B），只作早期预警，不能当因果结论。

**Q：怎么确认 Rust sidecar 算出的签名和我这里一致？**
A：起 sidecar 后在「系统诊断 → 执行内核」点「自检」：它用固定 query 与当前环境 Secret 各签一次，
比对结果（两侧均为 88 字符 base64）与往返耗时都会显示；`shadow` 模式下每笔真实签名也会持续比对，
`mismatched` 恒为 0 才建议切 `enforce`。

**Q：迁移目录后密钥解不开了？**
A：加密数据依赖 `data/.master_key`，搬家时必须与 `data/` 一起迁移，并让 `NANOQUANT_HOME`
指向新根目录；主密钥丢失则已保存的交易所/AI 密钥需要重新填写。

---

## ⚠️ 免责声明

本项目仅用于技术研究与学习，**不构成任何投资建议**。合约杠杆可能导致本金全部损失，
历史表现不代表未来收益。默认以模拟盘模式运行，切换到实盘需自行配置交易所密钥并显式确认。
使用本项目造成的任何损失，作者不承担责任。

---

## 📄 License

未附加开源许可证文件前，默认保留所有权利。如需用于商业用途，请联系作者获取授权。

<div align="center">
<sub>Built with FastAPI · Vue 3 · klinecharts · SQLite · Rust sidecar</sub>
</div>
