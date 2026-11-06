# exec-service（Rust 执行内核 sidecar）

当前阶段：**P2 影子模式** —— 只签名、不回传订单，用于和 Python 逐字节比对。

> 本机（Windows 开发机）**没有 Rust 工具链、没有 MSVC/MinGW 链接器、磁盘剩余不足 1GB**，
> 因此这份代码尚未在本地编译；请在目标 VPS 上构建（见下）。

## 构建（Linux VPS）

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
cd rust/exec-service
cargo test          # 含 3 个签名向量测试（与 Python 对齐）
cargo build --release
```

## 运行

```bash
EXEC_SECRET="$(cat /etc/nanoquant/exec_secret)" \
EXEC_SOCKET=/run/nanoquant/exec.sock \
./target/release/exec-service
```

systemd 片段：

```ini
[Service]
Environment=EXEC_SOCKET=/run/nanoquant/exec.sock
EnvironmentFile=/etc/nanoquant/exec.env   # EXEC_SECRET=...
ExecStart=/opt/nanoquant/exec-service
Restart=always
```

## 验证

```bash
# 协议 + 签名一致性（可对着真实 sidecar 跑）
python scripts/verify_sidecar.py --target unix:/run/nanoquant/exec.sock
```

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
  细节见 `docs/EXEC_PROTOCOL.md` §5 与 `docs/TASKS.md` #55。
- `src/sign.rs::normalize_pem` 已改为按 `-----...-----` 标记片段删除（旧状态机无法退出 `BEGIN`，
  会把 PEM 清空后静默回落 HMAC）；**该修复尚未在本机编译验证**，请在 VPS 上 `cargo test` 确认。

## 下一步（P3）

1. 在 VPS 上 `cargo test`（含单行 PEM 用例）→ `cargo build --release`；
2. 接入 `reqwest`（rustls）做 HTTP/2 连接池 + 启动预热；
3. 订单状态机（Planned → Sent → Acked → Filled），幂等键 `clientOrderId`；
4. 保护单并行下发（STOP + TP），参数走 Algo 接口；
5. 落库改由 Rust 单写线程组提交，Python 只读。
