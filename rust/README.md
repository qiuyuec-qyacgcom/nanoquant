# exec-service（Rust 执行内核 sidecar）

当前阶段：**P2 影子模式** —— 只签名、不回传订单，用于和 Python 逐字节比对。

已在 **Windows（x86_64-pc-windows-gnu）+ 回环 TCP** 上编译并验证（3 个向量测试全过，
真实 Ed25519 凭证下 `mismatched=0`、p99 < 1.2ms）。Linux 的 UDS 产物与 systemd 守护待 VPS 验证。

## 构建

Linux / macOS / VPS：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
cd rust/exec-service
cargo test          # 3 个签名向量（与 Python 对齐）
cargo build --release
```

Windows 开发机（无需 MSVC，用 GNU 目标）：

```powershell
# 1) rustup：默认 host 选 x86_64-pc-windows-gnu
rustup-init.exe --default-host x86_64-pc-windows-gnu --default-toolchain stable --profile minimal -y
# 2) 需要 MinGW-w64 的 binutils（dlltool/as/ar）：
#    rustup 自带 dlltool 会在生成 import lib 时失败，装一份 winlibs 并把 mingw64\bin 放到 PATH 前面
# 3) 构建（target 目录建议放仓库外，避免污染工作区）
$env:CARGO_TARGET_DIR='F:\tools\rust\target'
cargo test
cargo build --release
```

## 运行

Linux（UDS，推荐）：

```bash
EXEC_SECRET="$(cat /etc/nanoquant/exec_secret)" \
EXEC_SOCKET=/run/nanoquant/exec.sock \
./target/release/exec-service
```

Windows / 容器联调（回环 TCP；给了 `EXEC_TCP` 就只走 TCP）：

```powershell
$env:EXEC_SECRET='<base64 PKCS#8 或 HMAC secret>'
$env:EXEC_TCP='127.0.0.1:9101'
.\exec-service.exe
```

> Windows 没有 UDS：不设 `EXEC_TCP` 会直接返回明确错误，而不是静默不监听。
> Secret 建议用环境变量/`EnvironmentFile` 注入，不要写进命令行（会进进程列表）。

systemd 片段：

```ini
[Service]
Environment=EXEC_SOCKET=/run/nanoquant/exec.sock
EnvironmentFile=/etc/nanoquant/exec.env   # EXEC_SECRET=...
ExecStart=/opt/nanoquant/exec-service
Restart=always
RuntimeDirectory=nanoquant                # 顺便保证 /run/nanoquant 存在且属主正确
```

## 验证

```bash
# 自起 mock，协议 + 签名比对（无需真实 sidecar）
python scripts/verify_sidecar.py

# 对真实 sidecar：给出它实际使用的 Secret 才会做逐字节比对（否则只校验协议）
EXEC_SECRET=... python scripts/verify_sidecar.py --target unix:/run/nanoquant/exec.sock
EXEC_SECRET=... python scripts/verify_sidecar.py --target tcp:127.0.0.1:9101
```

实测（Windows 回环 TCP，release，真实测试网 Ed25519 凭证）：

| 场景 | 结果 |
|---|---|
| 三档模式 | `shadow` p50 0.50ms / p99 1.18ms；`enforce` p50 0.18ms / p99 0.40ms |
| 面板自检 | 两侧均 88 字符 base64，`compare_ms=0.12` |
| 错误密钥 | `mismatched` 立即增长，verdict 变「禁止切流」 |
| 进程被杀 | 3s 内降级回 Python 签名，静默 5s 后重试；重启自动重连 |
| 资源占用 | 二进制 0.91MB，常驻 RSS ~4MB，空闲 CPU 0 |

## 签名向量（Python 生成，Rust 单测断言）

| 项 | 值 |
|---|---|
| query | `symbol=BTCUSDT&side=BUY&type=MARKET&timestamp=1700000000000` |
| Ed25519 secret（base64 PKCS#8） | `MC4CAQAwBQYDK2VwBCIEIAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4f` |
| Ed25519 期望签名 | `9Z7/7rbpsRx/V335rbIjgYZc73mgxDfAwdrSux8fheFxw4zb9ncQdI0rnNtyYTNFNhpkMNoOYSba0xJe+hb2Aw==` |
| HMAC secret | `secret` |
| HMAC 期望签名 | `c5bee7ef23d0f08ff08fe04022f745e9ff389ec6ea2ad8203e0a096b238dcf36` |

## 目录

```
exec-service/
├── Cargo.toml
└── src/
    ├── main.rs    UDS/TCP 监听、JSON Lines 分发
    ├── model.rs   请求/响应结构
    └── sign.rs    HMAC + Ed25519 签名（含与 Python 对齐的向量测试）
```

## 当前状态

- Python 侧已把 sidecar 接进真实签名链路（`off / shadow / enforce` 三档 + 自检 + 诊断面板），
  细节见 `docs/EXEC_PROTOCOL.md` §5 与 `docs/TASKS.md` #55 / #56。
- 首次编译修掉 3 个缺陷：`UnixListener` 无条件导入（Windows 编译失败）、`stream.split()`
  方法解析失败（改用 `tokio::io::split`）、`handle` 缺 `Send` 约束（`tokio::spawn` 会拒绝）。
- `src/sign.rs::normalize_pem` 已按 `-----...-----` 片段删除并经单行 PEM 用例验证
  （旧状态机无法退出 `BEGIN`，会把 PEM 清空后静默回落 HMAC）。
- 换 Secret 后必须调 `POST /system/exec-sidecar/reset`：切换窗口内的在途请求会被判为不一致。

## 下一步（P3）

1. 在 VPS 上用 UDS 跑一次 `cargo test && cargo build --release`，补 systemd 守护与 `0600` 权限；
2. 接入 `reqwest`（rustls）做 HTTP/2 连接池 + 启动预热；
3. 订单状态机（Planned → Sent → Acked → Filled），幂等键 `clientOrderId`；
4. 保护单并行下发（STOP + TP），参数走 Algo 接口；
5. 落库改由 Rust 单写线程组提交，Python 只读。
