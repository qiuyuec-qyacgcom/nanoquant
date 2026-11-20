# 应急处置手册

> 出事时按这里做，不要临场想。所有动作在「参数配置」页都有入口，也可以用接口直接调。
> 设计原则：**一个动作解决一件事**，且都能撤销/重来。

---

## 一、要立刻离场：紧急全平

**什么时候用**：行情异常、程序行为不对、杠杆/仓位和预期不符、准备长期停手。

- 界面：参数配置 → 顶部「交易环境」面板 → **紧急全平**
- 接口：`POST /api/v1/engine/flatten`，body `{"confirm": "FLATTEN"}`
- 做的事（顺序固定）：
  1. **停引擎**（不再开新仓）
  2. 撤销该环境全部挂单与条件单
  3. 逐仓市价平仓；交易所侧有系统未记录的仓位会先补录再平
- 返回：`closed` / `failed` / `symbols`；`failed` 非空说明有仓位没平掉，**必须手动处理**
- 确认短语 `FLATTEN` 是防误触的，不是流程仪式

之后如果要恢复交易：先确认持仓为空 → 参数配置 → 启动引擎。

---

## 二、手动开的仓没有止损

**症状**：持仓列表里「止损/止盈」是 0。

- 对账发现 `sl=0 且 tp=0` 时会写一条警告事件
- 界面：参数配置 → 风控 → 日亏基准区块下方会列出「N 个仓位没有止损/止盈」→ **按 ATR 补挂保护单**
- 接口：`POST /api/v1/engine/positions/{id}/protect`
- 价格口径：当前周期 ATR × 策略里的 `sl_atr_mult` / `tp_atr_mult`
- 想让它自动做：打开 `risk.auto_protect_external`（默认关——自动替你挂单属于越权）

---

## 三、熔断停不下来 / 假日内亏损 -99%

**症状**：提示「日内亏损 -99.x% 超过上限」，但账户明明没亏。

**原因**：充值、重置模拟盘资金、换账户后，`day_start_equity` / `equity_peak` 还停在旧资金规模上。

- 界面：参数配置 → 风控 → **重置基准到当前权益**
- 接口：`POST /api/v1/engine/reset-anchors`
- 会同时清掉「日内亏损」与「最大回撤」两条：两者的判定依据（日亏基准、峰值）都被重置了，留着只会让引擎在已经恢复的状态下永久锁死。连亏 / 止损熔断属于定时自愈，本来就不落 `halted`

**日亏熔断会自己好，回撤不会**：

| 熔断类型 | 恢复方式 |
|---|---|
| 日内亏损 | 交易日切换（按 `engine.timezone_offset`）自动解除，无需人工 |
| 最大回撤 | 人工确认重置峰值，或改用「回撤峰值窗口」（见下） |
| 连亏冷静期 / 止损熔断 | 到点自动解除，只是拦那几轮 |

**小资金不要一直手动重置**：把 `风控 → 回撤峰值窗口` 设成 30（天），峰值只取最近 30 天权益曲线的最高点。默认 0 = 历史最高——10U 账户腰斩后会永远顶在 50% 熔断线上，除了改库没有别的出路。窗口模式下人工「重置基准」仍然有效：系统会把基准锚定在重置时刻，不会再去翻更早的高点。

如果面板显示「引擎已熔断: 最大回撤 …（需人工确认恢复）」，就是上表第二行。

---

## 四、每日自动备份没跑 / 备份目录是空的

**症状**：`参数配置 → 备份策略 → 每日自动备份` 是开着的，但 `data/backups/` 一直是空的。

**原因（已修）**：`backup.auto_daily` 曾经只是个存进数据库的开关，没有任何调度代码去读它。现在由 lifespan 里的后台任务每 15 分钟检查一次。

自查三步：

1. `GET /api/v1/backup/options` → `auto.enabled` 是否为 `true`
2. `auto.last_run_ts` 是否为 0：为 0 表示从未成功跑过；非 0 则是上次成功时间
3. `GET /api/v1/backup/jobs` → `created_by = "auto"` 的记录

失败不会静默：会写一条 `error` 级事件，并把重试时间推到 1 小时后（不是等第二天）。

---

## 五、凭证突然"不见了"

**症状**：之前填过 API Key，却提示未配置。

**原因**：API Key 用 `data/.master_key` 加密存储，而这个文件**刻意不进备份包**（避免备份泄露 = 密钥泄露）。换机器恢复后：密文在、密钥没了。

- 日志里会有一次 `凭证解密失败：主密钥与密文不匹配` 的告警
- 处理：把原机的 `data/.master_key`（或 `NANOQUANT_KEY_FILE` 指向的文件）放回 → 重启；找不回来就只能重填 API Key
- 备份时请**单独**保存这个文件，和备份包分开存放

---

## 六、参数被谁改了

参数配置页每个分组底部有「最近变更」，显示时间 / 用户 / 字段「旧值 → 新值」；密钥类字段只记录"改过"，不记录内容。

接口：`GET /api/v1/settings/audit?group=risk&limit=50`

---

## 七、日常检查清单（每周一次）

- [ ] `GET /api/v1/metrics` 能看到 `nanoquant_signals_last_1h` 非 0（长期为 0 = 策略与行情脱节）
- [ ] `nanoquant_last_cycle_age_seconds` 与 `cycle_interval_sec` 同量级（远大于就是扫描卡住）
- [ ] 告警总开关 `alert_enabled` 为 true，且 webhook / Telegram 收到过测试消息
  （参数配置 → 系统 → 发送测试告警）
- [ ] 持仓的止损/止盈都不为 0
- [ ] 备份能列出来，且 `.master_key` 单独存了一份
- [ ] 总览「委托流水」里没有长期停在 `NEW/PARTIALLY_FILLED` 的单（有 = 状态回查没跑通，台账不可信）
- [ ] 备份页 KPI「每日自动备份」显示的是**最近一天内**的时间戳（长期不更新 = 自动备份又哑了）
- [ ] **恢复演练**：`nanoquant ops backup-drill`（在运行库副本上跑：备份 → 删数据 → 还原 → 逐项比对；退出码非 0 就是有问题）

## 八、进程守护与数据库维护

### 8.1 防止"进程挂了没人知道"

症状（真实发生过）：页面还能打开，但引擎从某刻起再没写过心跳，停了两天半。

```text
systemd（推荐）：install.sh --systemd 生成 nanoquant.service，Restart=always 自动拉起
nohup（无 systemd）：scripts/start.sh --daemon；用 scripts/status.sh 检查，崩溃后需外部守护
```

systemd 用 `systemctl status nanoquant` / `journalctl -u nanoquant -f`；nohup 用 `scripts/status.sh` 与 `logs/stdout.log`。
请通过 `scripts/start.sh --daemon` 启动，避免手工重定向导致日志无限增长。

### 8.2 数据库瘦身

现状（2026-11 实测）：库 56.8MB，其中 34MB 是 2.7 万条 flat 信号的完整指标快照。
新数据已经改成精简快照（见 `rust/nanoquant/src/worker.rs` 的 flat 特征键集合），
**存量**用脚本压：

```bash
nanoquant ops cleanup-history                    # 演练：只报告能省多少
nanoquant ops cleanup-history --apply             # 压缩旧 flat 快照 + 删除废弃表
nanoquant ops cleanup-history --apply --vacuum    # 再重建库回收磁盘（必须先停服务）
```

实测结果：56.8MB → 16.0MB（flat 特征 34.1MB → 5.0MB），行数与 `integrity_check` 均无误。
`--vacuum` 会先做一次一致性备份（SQLite 在线备份 API，包含未 checkpoint 的 WAL），
再原子替换；磁盘可用不足时会拒绝执行而不是写到一半。

### 8.3 扫描并发

`引擎 → 每轮扫描标的 / 扫描并发`：串行时 12 个标的约 2.2s，改成 3 路并发约 0.8s。
并发**不增加**每轮请求数（权重不变），只是把 RTT 叠起来；标的池很大或网络抖动时再降到 2。

### 8.4 告警通道

系统诊断里有一项「告警通道」：未配置时是红色。在 `参数配置 → 系统` 填
Webhook（钉钉/飞书/企业微信机器人）或 Telegram Bot，然后用「发送测试告警」验证。
不配的话，熔断、下单失败、爆仓预警只会写进页面——没人会看。

### 8.5 初始密码会把后台锁住（这是故意的）

首次启动创建管理员时，密码来源按优先级取：

1. `NANOQUANT_ADMIN_PASSWORD` 环境变量；
2. 安装时通过 `--password-stdin` / `--password-file` 传入（systemd 模式写入 `/etc/nanoquant/nanoquant.env`，0600，成功后删除；nohup 模式只注入首次启动环境）；
3. 都没有 → **随机生成 16 位**，只打印在首次启动日志里（控制台 + `logs/nanoquant.log`）。

随机生成的那种会同时置 `security.password_is_default = true`，此时**除登录/改密/健康检查外，
所有 `/api/v1/*` 都返回 403**（`code: password_change_required`），前端会把你送到
「参数配置 → 安全」。改完密码标记自动置 false，一切恢复。

为什么不做成"只提示"：面板一旦映射到公网，一个能被猜到或从别处泄露的口令就是现成的后门。

- 安装时指定密码 → 不强制改密（是你自己设的）；随机生成 → 首次登录必须改
- 安装时指定的密码只在 0600 临时 env 文件或首次启动进程中存在，入账后立即删除
- 老库升级：密码仍是 `admin123` 且没有该标记时，启动会补标并强制改密
- 测试/CI 绕过：预置 `password_hash` + `password_is_default=false`（Rust 测试在临时库中构造）
- 忘记密码：`nanoquant ops reset-password`（随机生成新密码，并重新要求改密）

### 8.6 备份要有第二块盘

主库、日志、备份默认全在 `data/` 下，一块盘写满就是三样一起没。
设 `NANOQUANT_BACKUP_MIRROR=F:/nanoquant-backups` 后，每次备份会多抄一份到该目录
（镜像写失败只告警，不影响本地备份）。恢复时优先用镜像里那份 —— 它的存在本身
就说明当时主盘还没坏。
### 8.7 迁移期内核回滚（已结束）

P4 已完成 legacy 后端与虚拟环境退役，迁移期回滚开关不再存在。当前唯一内核为 Rust 二进制：
`rust/nanoquant/target/release/nanoquant`。启动/停止/状态请使用 `scripts/start.sh|stop.sh|status.sh`。
