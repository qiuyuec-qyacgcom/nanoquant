<div align="center">

# NanoQuant

**Rust 单栈 · Linux 部署 · 币安 U 本位永续量化终端**

<p>
  <img src="https://img.shields.io/badge/Rust-1.88%2B-B7410E?logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/Vue-3-4FC08D?logo=vuedotjs&logoColor=white" alt="Vue 3">
  <img src="https://img.shields.io/badge/TypeScript-3178C6?logo=typescript&logoColor=white" alt="TypeScript">
  <img src="https://img.shields.io/badge/SQLite-WAL-003B57?logo=sqlite&logoColor=white" alt="SQLite">
  <img src="https://img.shields.io/badge/Linux-only-2F6F44?logo=linux&logoColor=white" alt="Linux only">
  <img src="https://img.shields.io/badge/mode-demo%20default-blue" alt="Demo default">
</p>

**一个面向小资金的币安 U 本位永续量化系统：** 行情、信号、风控、AI 复核、执行与复盘
串成一条可解释、可审计的链路。默认模拟盘，参数热更新，当前主线为 **Rust 内核 + Rust sidecar**，
不再依赖 Python 运行时。

[特性](#-特性) · [界面](#-界面) · [快速开始](#-快速开始) · [参数](#-参数设置) · [架构](#-架构) ·
[交易链路](#-交易链路) · [运维](#-运维与观测) · [测试](#-测试) · [FAQ](#-faq)

</div>

---

## ✨ 特性

| 能力 | 说明 |
|---|---|
| 🦀 **Rust 单栈** | 主服务 `rust/nanoquant` 与执行 sidecar `rust/exec-service` 均为 Rust；运行期不依赖解释器 |
| 📈 **币安 U 本位永续** | 模拟盘走测试网、实盘走主网；合约规格来自币安在线合约表，数量/价格按步进格式化 |
| 🛡 **先风控再下单** | 单笔风险、保证金、杠杆、总敞口、同向持仓、日亏/回撤/连亏冷却多道闸门 |
| 🎯 **全局止盈止损** | 风控组可设置全局止损率/止盈率（按开仓价涨跌幅），每仓触发后整仓平仓，优先于 ATR 规则 |
| 🤖 **AI 复核** | OpenAI 兼容 / Anthropic 协议，只能同意、否决、降杠杆或收窄止损，不能放宽风控 |
| 🧬 **复盘与决策经验** | 平仓复盘沉淀心法；AI 方向判断留决策快照，回访打标后形成可检索的场景经验 |
| 🧾 **信号审计** | 每轮扫描的评分、AI 复核与拒绝原因可追溯；相同 flat 信号合并并保留真实扫描计数 |
| 🧪 **回测** | 与实盘共用策略/出场函数，支持参数网格与 walk-forward 样本外验证 |
| 📰 **资讯情绪** | 多源抓取、情绪指数、源体检；可选用 AI 摘要注入决策上下文 |
| 🎯 **标的池** | 支持币安真实中文合约（如 `牛来USDT`）；当前环境查不到时降级为自定义标的（仅记录、默认禁用） |
| 🖥 **前台 + 后台** | 公开品牌站（浅色象牙白/金）与登录后的交易终端（深色黑金）；响应式布局 |
| 🧩 **执行 sidecar** | 只签名不下单；`shadow` 旁路比对、`enforce` 不一致拒单，掉线自动降级 |
| 🧰 **Linux 一键部署** | `install.sh` 引导脚本；systemd 或 nohup；依赖检查、构建、权限收紧、健康检查与失败回滚 |
| 🔐 **安全与审计** | 密码 stdin/file/env 传入；systemd 临时 env 文件 0600 且成功后删除；主密钥/数据库不入备份包 |
| 🩺 **可观测** | 事件总线、死信补偿、运行态探针、`/metrics`、API 限频与执行内核自检 |

---

## 🖥 界面

前台公开（无需登录），后台需登录。路由使用 hash 模式。

| 端 | 页面 | 内容 |
|---|---|---|
| 前台 | `/` | 首页、标的池 K 线、运行状态、核心能力 |
| 前台 | `/charts` | 池内标的 K 线工作台、周期切换、24H 概览 |
| 前台 | `/insights` | 资讯流、情绪指数、脱敏观点 |
| 前台 | `/about` | 执行链路、技术栈与风险声明 |
| 后台 | `/admin/dashboard` | 权益曲线、账户、持仓、事件流、引擎控制 |
| 后台 | `/admin/signals` | 信号审计、拒绝原因、扫描密度 |
| 后台 | `/admin/symbols` | 标的池：币安合约校验、真实中文合约、自定义标的、删除确认 |
| 后台 | `/admin/strategy` | 参数配置：交易执行 / 策略 / 风控 / 交易所 / 资讯 / 通知与系统 / 备份 / 安全 |
| 后台 | `/admin/backtest` | 回测、参数网格、walk-forward |
| 后台 | `/admin/ai` | 模型接入、诊断、决策演练、思考强度 |
| 后台 | `/admin/news` | 资讯情绪与源检测 |
| 后台 | `/admin/evolution` | 心法库、注入预览、效果对照 |
| 后台 | `/admin/journal` | 决策日志、回访打标、经验库 |
| 后台 | `/admin/backup` | 备份 / 还原 / 下载 / 上传 |
| 后台 | `/admin/dead-letters` | 落库死信查看与重放 |
| 后台 | `/admin/system` | 诊断、执行内核、告警、归档与日志 |

视觉概念稿见 `docs/design/ui-redesign/`（6 张截图 + HTML 预览）。

---

## 🚀 快速开始

### 前置要求

| 依赖 | 版本 | 说明 |
|---|---|---|
| Rust | **1.88+** | 构建主服务与 sidecar；运行期不需要解释器 |
| Node.js | **20+** | 仅构建前端时需要；已有 `frontend/dist` 时可用 `--no-frontend` |
| 系统 | **Linux** | Debian/Ubuntu、RHEL/Fedora、Alpine、Arch、openSUSE |
| 构建依赖 | gcc / make / perl / curl / git / pkg-config / acl | 默认自动安装；离线环境可预装 |
| 磁盘 | ≥ 2 GB 可用 | SQLite、日志、备份均在 `data/` 下 |

### 方式一：一条命令在线安装

```bash
if [ -f /usr/bin/curl ];then curl -fsSL --proto '=https' --tlsv1.2 \
  https://raw.githubusercontent.com/qiuyuec-qyacgcom/nanoquant/main/install.sh -o install.sh;else \
  wget --https-only -O install.sh \
  https://raw.githubusercontent.com/qiuyuec-qyacgcom/nanoquant/main/install.sh;fi
bash install.sh --systemd
```

首次管理员密码可用环境变量/标准输入传入，避免 shell 历史：

```bash
NANOQUANT_ADMIN_PASSWORD='你的密码' sudo -E bash install.sh --systemd
```

`install.sh` 下载代码后转交 `scripts/install.sh`：自动安装缺失的构建工具、Rust 与 Node.js，
检查工具链、构建 Rust/前端，创建 `data/`、`logs/` 与权限、按需创建 `nanoquant` 系统用户并生成 systemd unit，
最后通过 `/api/v1/health` 验证。仓库位于 `/root` 下也可直接安装，脚本会自动为服务用户授予路径穿越权限。
失败时会尝试恢复旧二进制/旧 unit。

### 方式二：本地脚本

```bash
sudo bash scripts/install.sh --systemd   # systemd：自动安装缺失工具并创建 nanoquant 用户
bash scripts/install.sh --no-systemd                    # 无 systemd：nohup
bash scripts/status.sh                                  # 查看状态
bash scripts/stop.sh                                    # 停止
bash scripts/start.sh --daemon                          # 手动后台启动
bash scripts/update.sh --check                          # 只读检查更新
```

### 方式三：Docker

```bash
docker compose up -d
# http://127.0.0.1:8899
```

### 方式四：手动

```bash
cargo build --release --locked --manifest-path rust/nanoquant/Cargo.toml
npm --prefix frontend ci
npm --prefix frontend run build
scripts/start.sh        # 前台启动；生产用 systemd 或 --daemon
```

### 安装验证与首次登录

```bash
curl http://127.0.0.1:8899/api/v1/health
```

默认账号 `admin`。密码按以下顺序确定：
1. 安装/启动时传入的 `NANOQUANT_ADMIN_PASSWORD`（systemd 模式下临时 env 文件权限 0600，成功后删除）；
2. 通过安装脚本部署且未传入时，安装脚本自动生成 20 位随机密码，成功后只在终端打印一次，不写入安装日志。

已有管理员密码时安装脚本不会覆盖；需要重置请使用 `nanoquant ops reset-password`。
首次登录后必须先改密；改密前除登录/改密/健康检查外接口返回 `403 password_change_required`。
忘记密码：`nanoquant ops reset-password`。

### 常用环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `NANOQUANT_HOME` | 仓库根目录 | 运行目录；迁移时 `data/` 与 `.master_key` 必须一起搬 |
| `NANOQUANT_PORT` / `NANOQUANT_HOST` | `8899` / `0.0.0.0` | 监听地址 |
| `NANOQUANT_DB` | `data/nanoquant.db` | SQLite 数据库 |
| `NANOQUANT_FRONTEND` | `frontend/dist` | 前端产物 |
| `NANOQUANT_LOG_DIR` | `logs` | 日志目录 |
| `NANOQUANT_KEY_FILE` | `data/.master_key` | 加密密钥；丢失后已保存的 API Key 无法解密 |
| `NANOQUANT_ADMIN_PASSWORD` | 空 | 仅首次建管理员时生效 |
| `EXEC_SOCKET` | `/run/nanoquant/exec.sock` | sidecar UDS 地址；仅由部署环境提供 |
| `EXEC_SECRET` | 无 | sidecar 签名 Secret，生产放权限 0600 的 `EnvironmentFile` |
| `NANOQUANT_BACKUP_MIRROR` | 空 | 备份异盘副本目录，如 `/mnt/nanoquant-backups` |

---

## 🎛 参数设置

参数分 8 组，全部可在后台热更新：

1. **交易执行**：周期、最大持仓、交易时段、时区、下单方式
2. **策略**：趋势/均值回归模式、周期、入场过滤、ATR 止损止盈、最小盈亏比、移动止损、保本、部分止盈、ADX；AI 在配置了可用 provider 时自动复核
3. **风控**：单笔风险、保证金、默认/最小/最大杠杆、总敞口、同向持仓、全局止盈止损、日亏/回撤/连亏熔断、高级风控、执行成本
4. **交易所**：测试网/主网地址、备用地址、代理与 API 凭证
5. **资讯**：抓取节奏、源、情绪与 AI 摘要
6. **通知与系统**：日志、保留、Webhook/Telegram、限频与指标
7. **备份**：保留份数、加密、自动备份、日志
8. **安全**：账号、登录要求、会话时长、失败锁定

### 全局止盈止损

- `global_stop_loss_pct` / `global_take_profit_pct`，单位 `%`，默认 `0` 表示关闭。
- 按**开仓价涨跌幅**计算（不是叠加杠杆后的 ROE）；多头/空头自动取方向。
- 每轮管仓先检查全局规则，再检查 ATR 止损止盈、部分止盈、ROI 阶梯、移动止损与信号反转。
- 触发后整仓平仓，原因写入 `global_stop_loss` / `global_take_profit`。

### 中文标的

- 前端保留中文，后端按 Unicode 校验 `...USDT` 名称。
- 当前交易环境的币安合约表存在该符号时，按真实合约正常启用并采用交易所规格。
- 合约表没有时，降级为 `tier=custom`、`enabled=0` 的**自定义标的**，仅记录，不参与行情与下单。
- 模拟盘找不到真实中文合约时，可切到实盘环境后再添加。

---

## 🧭 架构

```text
nanoquant/
├── rust/nanoquant/        # 主服务：API、引擎、风控、AI、回测、SQLite、SPA 托管
├── rust/exec-service/     # 执行 sidecar：只签名、不下单
├── frontend/              # Vue 3 + TypeScript；site/ 公开站，views/ 后台
├── scripts/               # install / start / stop / update / setup_sidecar
├── deploy/                # systemd unit 模板与部署说明
├── docs/                  # RUNBOOK / MAINNET_DRILL / EXEC_PROTOCOL / 设计稿 / 归档
├── .github/workflows/     # Rust、前端、Playwright、零解释器门禁
├── Dockerfile
└── docker-compose.yml
```

**数据层**：SQLite（WAL），Rust `rusqlite` 访问；运行态、事件、信号、成交、心法、经验全部可追溯。
**写入模型**：单写队列串行落库，失败进入死信补偿；过期数据归档后清理。
**前端**：Vue 3 + TypeScript + ECharts；公开站浅色、后台深色黑金，轮询在页面隐藏时暂停，恢复可见立即补刷。

---

## ⚙ 交易链路

引擎按配置周期扫描标的池，任一步不通过都不开仓，原因写入信号审计：

1. **取价**：币安公共行情、K 线、资金费率、持仓量；K 线/费率/持仓量有短 TTL 缓存，前台 K 线走 WS。
2. **信号**：趋势、动量、量能、资金费率、持仓量加权；按模式走趋势或均值回归。
3. **准入**：ATR、量比、24H 成交额、EMA/ADX 等过滤。
4. **风控**：权益、日亏、回撤、连亏、冷却、总敞口、同向持仓、最小名义额。
5. **仓位**：`min(风险预算 ÷ 止损%, 保证金上限, 可用余额)`，按步进取整。
6. **AI 复核**：只允许收紧，不允许放宽；失败 fail-closed。
7. **下单/管仓**：市价 + 保护单；全局止盈止损、ATR、ROI 阶梯、移动止损、保本、信号反转。

平仓后可进入复盘、心法沉淀与决策经验回访。

---

## 🧩 执行内核 sidecar（兼容诊断）

主服务以 Rust 内核签名为准，参数页不再提供执行内核模式/地址。sidecar 只作为可选的独立签名与自检通道，
由部署环境变量 `EXEC_SOCKET` / `EXEC_SECRET` 提供连接信息，不在后台参数页切换 `off/shadow/enforce`。

sidecar 只持有 Secret，不接触账户、持仓与订单。协议见 `docs/EXEC_PROTOCOL.md`。
systemd 模式下主服务与 sidecar 必须使用同一用户，才能访问权限 0600 的 UDS；
`scripts/setup_sidecar.sh` 默认复用已存在的 `nanoquant` 用户。

---

## 🛠 运维与可观测

```bash
# 主网演练前只读预检
nanoquant ops preflight --yes

# 零解释器门禁（仓库 / 运行时 / 镜像 / 文本 / 归档）
nanoquant ops zero-python --scope all --manifest image-manifest.txt

# 契约与签名检查
nanoquant ops contract-diff --baseline .codeartsdoer/specs/rust_full_migration/baseline/contract_baseline.json
nanoquant ops verify-sidecar
nanoquant ops reconcile --self-test
```

- **诊断**：数据库、写队列、交易所通道、引擎心跳、AI、存储、sidecar 一键体检。
- **事件**：warn/error 入库并实时推送；支持 Webhook / Telegram、冷却与静默时段。
- **死信**：写库失败可查看、重放或忽略；`/system/write-health` 查看积压。
- **归档**：过期事件/信号先压缩再删除。
- **备份**：选择性打包、SHA-256 校验、还原前快照；主密钥不入包。
- **指标**：`/metrics` 提供 Prometheus 文本，不含账户与密钥。
- **部署**：systemd 提供开机自启、崩溃拉起与最小权限硬化；没有 systemd 时用 nohup。

生产更新：

```bash
bash scripts/update.sh --check
sudo bash scripts/update.sh
```

有未提交改动时更新脚本会中止；数据、数据库与主密钥不会被覆盖。

---

## 🧪 测试

```bash
cargo fmt --all --manifest-path rust/nanoquant/Cargo.toml --check
cargo clippy --all-targets --manifest-path rust/nanoquant/Cargo.toml -- -D warnings
cargo test --release --locked --manifest-path rust/nanoquant/Cargo.toml
cargo test --release --locked --manifest-path rust/exec-service/Cargo.toml

npm --prefix frontend run typecheck
npm --prefix frontend test
npm --prefix frontend run build
```

前端 e2e（需先启动服务）：

```bash
npm --prefix frontend run e2e
```

CI 覆盖 Rust 双 crate 的 fmt/clippy/test、前端 typecheck/test/build、Playwright 冒烟、
`bash -n` 脚本检查、零解释器门禁与 `contract-diff`。

---

## 🗺 路线图

已完成：

- Rust 主服务 + Rust sidecar 单栈
- Linux systemd/nohup 安装与失败回滚
- 全局止盈止损与参数收敛
- 真实中文币安合约 + 自定义标的降级
- 前台浅色 / 后台深色黑金、无感刷新
- 回测、walk-forward、信号审计、资讯情绪、心法/决策经验
- 死信补偿、事件总线、指标、备份与主网预检

下一步：

- 多账户 / 子账户隔离
- OKX / Bybit 等更多交易所适配
- 更长周期的样本外验证与滑点校准
- 心法健康度时序可视化

---

## ❓ FAQ

**Q：启动后面板报请求失败 / `403 password_change_required`？**
A：首次密码只允许登录与改密；登录后按提示改密即可。忘记密码用 `nanoquant ops reset-password`。

**Q：构建失败？**
A：确认 Rust 1.88+、Node.js 20+ 与 gcc/make/perl/curl 已安装；重新执行
`sudo bash scripts/install.sh`，脚本会自动补齐缺失工具。

**Q：加不进标的？**
A：真实币安合约会在当前环境合约表中校验；模拟盘没有的中文合约会降级为仅记录的自定义标的。
需要交易请切到有该合约的环境，或添加对应的真实合约。

**Q：引擎启动了但不开仓？**
A：检查标的池是否为空、交易时段、冷却/熔断、当前环境 API 凭证、最小名义额与可用保证金；
「开仓检查」页会逐项给出原因。

**Q：为什么 K 线是直线或 0？**
A：常见于测试网无成交合约或小价格币精度问题；前端会按价格自适应精度，并对静态/空数据给出提示。

**Q：迁移目录后 API Key 解不开？**
A：必须同时迁移 `data/` 与 `data/.master_key`，并让 `NANOQUANT_HOME` 指向新目录。

更多运维细节：`docs/RUNBOOK.md`；主网演练：`docs/MAINNET_DRILL.md`；
sidecar 协议：`docs/EXEC_PROTOCOL.md`；历史迁移记录：`docs/archive/history/INDEX.md`。

---

## ⚠️ 免责声明

本项目仅用于技术研究与学习，不构成任何投资建议。合约杠杆可能导致本金全部损失，
历史表现不代表未来收益。默认模拟盘运行；切到实盘前请自行配置 API 凭证、完成演练并显式确认。
使用本项目造成的任何损失，作者不承担责任。

## 📄 License

未附加开源许可证文件前，默认保留所有权利。商业用途请联系作者获取授权。

<div align="center">
  <sub>Rust · Vue 3 · SQLite · Linux · Binance USDT-M</sub>
</div>