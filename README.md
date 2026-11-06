<div align="center">

# NanoQuant

**小资金币安合约量化终端 · 最低 1 USDT 起 · 1–100x 杠杆 · AI 辅助决策 · 自进化心法**

<p>
  <img src="https://img.shields.io/badge/python-3.11%2B-3776AB?logo=python&logoColor=white" alt="Python">
  <img src="https://img.shields.io/badge/FastAPI-009688?logo=fastapi&logoColor=white" alt="FastAPI">
  <img src="https://img.shields.io/badge/Vue-3-4FC08D?logo=vuedotjs&logoColor=white" alt="Vue 3">
  <img src="https://img.shields.io/badge/TypeScript-3178C6?logo=typescript&logoColor=white" alt="TypeScript">
  <img src="https://img.shields.io/badge/SQLite-003B57?logo=sqlite&logoColor=white" alt="SQLite">
  <img src="https://img.shields.io/badge/mode-demo%20default-blue" alt="Demo default">
</p>

**一个面向极小资金的币安 U 本位永续合约量化系统：** 把行情、信号、风控、AI 复核与自进化复盘
串成一条**可解释、可审计**的执行链。默认模拟盘模式，所有参数热更新，支持选择性备份与还原。

[快速开始](#-快速开始) · [交易逻辑](#-交易逻辑) · [架构](#-架构) · [配置](#-默认参数) · [API](#-api-概览)

</div>

---

## ✨ 特性

| 能力 | 说明 |
|---|---|
| 💸 **1 USDT 起跑** | 按交易所步进计算真实最小名义额，取整不足即明确拒绝，而不是发出必然失败的订单 |
| 📈 **1–100x 杠杆** | 单笔风险预算反推名义额，再被保证金上限截断；杠杆只能逐层封顶，不会越权 |
| 🤖 **AI 辅助开平仓** | OpenAI 兼容协议，单模型 / 多席位议会；输出强制 JSON 契约，**只能收紧风控、不能放宽** |
| 🧬 **自进化心法** | 每笔平仓自动复盘，结论经防污染审计后沉淀为心法；心法按健康度排序回注后续决策，可被优化替代 |
| 🧾 **信号审计** | 每一轮扫描的五因子评分、AI 复核与最终裁决；拒绝原因按 Top 分布，"为什么没开仓"一页看尽 |
| 📰 **资讯与情绪** | 11 个源（含币安广场 / 公告），情绪指数与币种共现，可选注入 AI 上下文 |
| 🌐 **前台 / 后台双端** | 前台公开站点（首页 / 观点 / 关于，响应式）+ 后台管理终端（需登录） |
| 📈 **前台 K 线（跟随标的池）** | 前台只展示后台已添加的合约，严格按添加顺序排列；空池显示「未添加标的」，增删 10 秒内自动同步，数据直接取币安实时接口 |
| 🎯 **astra 规则标的池** | 添加前用币安在线合约表校验（USDT 永续 / 交易中），分层派生杠杆与止损，去重 + 容量上限，删除需确认短语且持仓禁删 |
| 🧩 **执行内核 sidecar** | Rust 只签名不下单：`shadow` 旁路逐字节比对（零阻塞）、`enforce` 不一致直接拒单；掉线自动降级 + 5s 静默 |
| 💾 **选择性备份** | 任意勾选配置组与数据文件，SHA-256 校验，还原前自动快照，主密钥不入包 |
| 🧰 **一键部署** | `install.sh` / `install.ps1` / Docker Compose，三种方式等价 |

---

## 🖥 界面

前台（公开，无需登录）与后台（`/admin`，需登录）视觉同源，移动端自动切换底部 Tab。

| 端 | 页面 | 内容 |
|---|---|---|
| 前台 | `/` | 状态条、标的池 K 线工作区、核心能力、心法、资讯 |
| 前台 | `/charts` | 行情终端：池内标的 K 线（按添加顺序）、周期切换、24H 概览 |
| 前台 | `/insights` | 资讯流、情绪指数、脱敏心法 |
| 前台 | `/about` | 开仓链路、技术栈、风险声明 |
| 后台 | `/admin/dashboard` | 权益曲线、账户、持仓、信号审计、事件流、引擎控制 |
| 后台 | `/admin/onboarding` | 开仓检查：7 项前置条件 + 每标的下单量 |
| 后台 | `/admin/signals` | 信号审计（评分 / AI / 裁决 / 拒绝原因分布） |
| 后台 | `/admin/symbols` | 标的池（币安合约表校验、分层/精度/最小下单、删除确认短语） |
| 后台 | `/admin/news` | 资讯情绪 + 源检测 |
| 后台 | `/admin/ai` | 模型接入、诊断、决策演练 |
| 后台 | `/admin/strategy` | 交易环境（模拟盘 / 实盘）+ 引擎/风控/策略/数据源等分区参数 |
| 后台 | `/admin/evolution` | 心法库、注入预览、复盘记录 |
| 后台 | `/admin/backup` | 备份 / 还原 / 下载 |
| 后台 | `/admin/system` | 系统诊断、慢 SQL、运行日志 |

---

## 🚀 快速开始

### 方式一：一键脚本（推荐）

```bash
# Linux / macOS
bash scripts/install.sh     # 创建 venv、装依赖、构建前端、初始化数据库
bash scripts/start.sh       # 启动，默认 http://127.0.0.1:8899
bash scripts/status.sh      # 查看状态
bash scripts/stop.sh        # 停止
```

```powershell
# Windows PowerShell
powershell -ExecutionPolicy Bypass -File scripts\install.ps1
powershell -File scripts\start.ps1
```

### 方式二：Docker

```bash
docker compose up -d
# 访问 http://127.0.0.1:8899
```

### 方式三：手动

```bash
python -m venv .venv && source .venv/bin/activate   # Windows: .venv\Scripts\activate
pip install -r backend/requirements.txt
cd frontend && npm install && npm run build && cd ..

cd backend
python -m nanoquant        # 首次启动自动建库并创建 admin
```

首次登录：`admin / admin123`（登录后会被要求立即改密）。

> 端口、数据目录等均在 `backend/nanoquant/config.py` 中，可用环境变量覆盖。

---

## 🧭 架构

```
nanoquant/
├── backend/nanoquant/
│   ├── engine/          # 交易引擎：主循环、信号、风控、指标
│   ├── exchange/        # 币安公共行情 + 签名客户端、模拟盘(测试网)/实盘执行层、精度与限频
│   ├── ai/              # 多模型调用、JSON 契约决策、议会聚合、诊断
│   ├── evolution/       # 复盘、心法库、防污染审计
│   ├── news/            # 资讯源适配、情绪打分、聚合与源体检
│   ├── services/        # 配置热更新、运行态、事件总线、标的池、备份、准入体检
│   ├── api/routes/      # auth / system / symbols / trade / ai / evolution / news / backup / public / ws
│   └── main.py          # FastAPI 装配 + SPA 托管
├── frontend/src/
│   ├── views/site/      # 公开前台（首页 / 观点 / 关于）
│   ├── views/           # 后台页面
│   ├── components/      # EChart / EnvironmentSwitch / Modal / EventFeed ...
│   └── layouts/         # SiteLayout（前台）/ ShellLayout（后台）
├── rust/exec-service/   # 执行内核 sidecar（P2 影子签名：UDS + JSON Lines）
├── scripts/             # install / start / stop / status（sh + ps1）
├── tests/               # smoke / evolution / ai / news 探测 + 本地 Mock 模型
├── docs/RESEARCH.md     # 开源量化系统调研与融合决策
├── Dockerfile
└── docker-compose.yml
```

**数据层**：SQLite（WAL）+ SQLAlchemy 2.x。所有运行态（冷却锁、连亏计数、日切锚点、阶段耗时）存
`data/engine_state`，重启不丢；事件、信号、成交、心法全部入库可追溯。

---

## ⚙ 交易逻辑

引擎每 5 秒一轮，七步链路，**任何一步不通过都不开仓，原因写入信号审计**：

```
① 取价      引擎内部直接调币安公共接口（5–30m K 线 + 资金费率 + 持仓量），无本地行情模块
② 信号      五因子加权得分 ∈ [-1,1]
            趋势0.32 / 动量0.26 / 量能0.18 / 资金费率0.12 / 持仓量0.12
            准入：ATR%∈[0.05,4]、量比≥0.80、24h成交额≥300万
            结构确认：多头需 EMA9>EMA21，空头反之（避免逆势抄底）
③ 风控      全局：权益≥0.5U、日内亏损≤25%、回撤≤50%、连亏<4笔、无熔断
            单标的：未持仓、持仓数<2、无冷却、总敞口<2000%
④ 仓位      名义额 = min( 权益×10% ÷ 止损%, 权益×50%×杠杆 )
            数量 = 名义额 ÷ 价，按步进向下取整；低于最小可下单量 → 拒绝
⑤ AI 复核  只能「同意 / 否决 / 降杠杆 / 收窄止损」，不能放宽任何风控
            解析失败、超时、调用失败 → fail-closed 跳过
⑥ 下单      市价 + 止损/止盈保护单（模拟盘走币安测试网，实盘走主网；与数量解耦）
⑦ 持仓管理  硬止损 / 止盈 / 阶梯止盈 / 移动止损 / 超时 / 信号反转
            浮盈 R≥1.0 先部分止盈（默认 50%），剩余仓位止损上移保本
```

平仓后自动进入复盘：规则兜底 + AI 复盘 → 防污染审计 → 心法入库（带健康度、版本、替代关系）
→ 每 N 笔触发全局复盘 → 心法回注下一次 AI 决策。

---

## 🔑 默认参数

| 组 | 参数 | 默认 |
|---|---|---|
| 引擎 | 周期 / 同时持仓 / 每轮扫描 / 时段 / 交易环境 | 5s / 2 / 12 / 全天 / demo 模拟盘 |
| 风控 | 单笔风险 / 默认杠杆 / 保证金占比 / 总敞口 | 10% / 20x / 50% / 2000% |
| 风控 | 日内亏损 / 回撤 / 连亏上限 / 冷静期 | 25% / 50% / 4 笔 / 10 分钟 |
| 策略 | 周期 / 阈值 / 量比 / ATR% / 最长持仓 | 5m / 0.30 / 0.80 / 0.05–4 / 180 分钟 |
| 执行 | 盘口点差闸门 / 时间阶梯止盈 | 8 bps / 0min 0.8%、15min 0.6%、30min 0.45%、60min 0.3%、120min 0.2% |
| AI | 模式 / 最低置信 / 调用上限 / 月预算 | single / 0.45 / 30 次每小时 / $3 |

> 1 USDT 买不了 BTC：BTCUSDT 最小名义额以币安合约表为准（当前约 50–100 USDT）。小资金请选 DOGE / SOL / ETH 等小面值标的，
> 或提高保证金比例 / 杠杆。面板会在「开仓检查」和信号审计里给出这一原因。

---

## 🔌 API 概览

统一前缀 `/api/v1`，后台接口需 `Authorization: Bearer <token>`；`/api/v1/public/*` 为公开只读。

| 分组 | 端点 |
|---|---|
| 认证 | `POST /auth/login`、`GET /auth/me`、`POST /auth/password` |
| 标的池 | `GET /symbols`、`GET /symbols/available`、`POST /symbols`、`POST /symbols/bulk`、`PATCH /symbols/{symbol}`、`DELETE /symbols/{symbol}?confirmation=REMOVE%20<SYMBOL>` |
| 交易 | `GET /engine/status`、`POST /engine/control`、`POST /engine/cycle-now`、`GET /engine/matrix`、`GET /engine/positions`、`POST /engine/positions/{id}/close`、`GET /engine/admission`、`/engine/signals`、`/engine/orders`、`/engine/trades`、`/engine/equity`、`/engine/stats` |
| AI | `GET/POST /ai/providers`、`POST /ai/providers/{id}/test`、`POST /ai/providers/{id}/diagnose`、`GET /ai/providers/{id}/models`、`POST /ai/analyze`、`POST /ai/decision-preview`、`GET /ai/logs`、`GET /ai/usage` |
| 自进化 | `GET /evolution/mantras`、`PATCH/DELETE /evolution/mantras/{id}`、`GET /evolution/injection`、`GET /evolution/runs`、`POST /evolution/review/{trade_id}`、`POST /evolution/global-review`、`GET /evolution/stats` |
| 资讯 | `GET /news`、`POST /news/refresh`、`GET /news/overview`、`GET /news/diagnostics`、`POST /news/ai-summary` |
| 备份 | `GET /backup/options`、`POST /backup/create`、`GET /backup/jobs`、`POST /backup/{id}/restore`、`DELETE /backup/{id}`、`GET /backup/{id}/download` |
| 系统 | `GET /health`、`GET /system/info`、`GET /system/diagnostics`、`GET /system/events`、`GET /system/logs`、`GET/PUT /settings/{group}`、`POST /settings/{group}/reset` |
| 执行内核 | `GET /system/exec-sidecar`、`POST /system/exec-sidecar/self-test`、`POST /system/exec-sidecar/reset` |
| 公开 | `GET /public/config`、`/public/overview`、`/public/pool`（按添加顺序）、`/public/klines`、`/public/klines/batch`（仅池内合约）、`/public/news`、`/public/mantras`、`/public/performance` |
| 实时 | `WS /ws?token=...`（事件流）、`WS /ws/klines?symbol=&interval=`（K 线推送, 服务端复用币安 WebSocket 流, 不占 REST 权重） |

公开接口严格脱敏：不含账户余额、持仓、订单、密钥与心法正文；绩效默认关闭，开启后也只给比例。

---

## 🧪 测试

```bash
cd backend
python -m pytest tests/test_contracts.py -q   # 语义契约：出场/阶梯止盈/部分止盈/回测（不联网）
python tests/smoke_test.py        # 端到端：登录→标的池→引擎→信号→备份→SPA
python tests/evolution_test.py    # 自进化闭环：平仓→复盘→心法→去重→防污染
python tests/ai_test.py           # AI 链路：诊断/测试/演练/议会（需本地 Mock 模型）
python tests/mock_llm.py          # 启动本地 Mock OpenAI 兼容服务
python tests/news_probe.py        # 资讯源体检
```

> CI：`.github/workflows/ci.yml` 在 push / PR 时跑 `compileall + pytest`（后端）与 `npm ci && npm run build`（前端）。

---

## 🗺 路线图

- [ ] 多账户 / 子账户隔离
- [ ] 回测引擎（历史 K 线 + 心法版本对比）
- [ ] WebSocket 行情订阅与增量 K 线
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

**Q：心法会越来越不可控吗？**
A：不会。心法入库前经防污染审计（拦截提高杠杆、取消止损等危险结论），并带健康度与版本；
全局复盘会淘汰低分心法，新心法以"替代"关系覆盖旧结论。

---

## ⚠️ 免责声明

本项目仅用于技术研究与学习，**不构成任何投资建议**。合约杠杆可能导致本金全部损失，
历史表现不代表未来收益。默认以模拟盘模式运行，切换到实盘需自行配置交易所密钥并显式确认。
使用本项目造成的任何损失，作者不承担责任。

---

## 📄 License

未附加开源许可证文件前，默认保留所有权利。如需用于商业用途，请联系作者获取授权。

<div align="center">
<sub>Built with FastAPI · Vue 3 · klinecharts · SQLite</sub>
</div>
