//! P3-4：sidecar 台账落地——**单写线程 + 批量事务**。
//!
//! 阶段目标：把「写库」从每个请求的同步路径上摘下来，改由一个专职写线程批量提交，
//! 与 Python 侧 `services/db_writer.py` 的取舍同源（下单关键路径只付一次入队）。
//!
//! 本阶段**只落 sidecar 自己的订单台账**（独立库文件，默认 `exec-ledger.db`）：
//! - 不动 Python 的 21 张主表，零冲突、可随时删库重建；
//! - 主库仍在 Python 手里（P3-4 的「Python 只读」要等台账被验证为权威源之后再谈）。
//!
//! 三条安全边界（对齐 Python 的写队列）：
//! 1. **有界队列**：满了不阻塞调用方，直接丢弃并计数（台账是旁路，不能拖住下单）；
//! 2. **批量事务**：一次 `BEGIN..COMMIT` 提交一批，摊薄 fsync；
//! 3. **失败不吞**：写失败计数 + 记录最后错误，由 `store_status` 暴露。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};
use serde::Serialize;

/// 一批最多提交多少条（摊薄 fsync，又不让单批过大拖长事务）
const BATCH_MAX: usize = 64;
/// 队列积压上限：满了丢弃（台账是可观测性，不是交易正确性的必要条件）
const QUEUE_CAP: usize = 8192;

/// 一条待落库的台账记录。
#[derive(Debug, Clone)]
pub struct LedgerRow {
    pub client_order_id: String,
    pub symbol: String,
    pub state: String,
    pub filled_qty: f64,
    pub avg_price: f64,
    pub ts: u64,
}

/// 写线程运行状态（序列化进 health / 面板）。
#[derive(Debug, Default, Serialize)]
pub struct StoreStatus {
    pub enabled: bool,
    pub path: String,
    pub pending: usize,
    pub written: u64,
    pub batches: u64,
    pub dropped: u64,
    pub errors: u64,
    pub last_error: String,
    pub last_batch_ms: u64,
}

struct Counters {
    written: AtomicU64,
    batches: AtomicU64,
    dropped: AtomicU64,
    errors: AtomicU64,
    last_batch_ms: AtomicU64,
    last_error: Mutex<String>,
    pending: AtomicU64,
}

impl Counters {
    fn new() -> Self {
        Self {
            written: AtomicU64::new(0),
            batches: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            last_batch_ms: AtomicU64::new(0),
            last_error: Mutex::new(String::new()),
            pending: AtomicU64::new(0),
        }
    }
}

pub struct Store {
    enabled: bool,
    path: String,
    tx: Mutex<Option<SyncSender<LedgerRow>>>,
    counters: Arc<Counters>,
    stopping: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Store {
    /// `path` 为空表示不落库（纯内存台账，行为与 P3-2 完全一致）。
    pub fn open(path: &str) -> Self {
        if path.trim().is_empty() {
            return Self {
                enabled: false,
                path: String::new(),
                tx: Mutex::new(None),
                counters: Arc::new(Counters::new()),
                stopping: Arc::new(AtomicBool::new(false)),
                handle: Mutex::new(None),
            };
        }
        match Self::spawn_writer(path) {
            Ok((tx, counters, stopping, handle)) => Self {
                enabled: true,
                path: path.to_string(),
                tx: Mutex::new(Some(tx)),
                counters,
                stopping,
                handle: Mutex::new(Some(handle)),
            },
            Err(exc) => {
                // 打不开就降级为纯内存：台账是旁路，不能因为磁盘问题拒绝启动
                eprintln!("ledger store disabled (open failed): {exc}");
                Self {
                    enabled: false,
                    path: String::new(),
                    tx: Mutex::new(None),
                    counters: Arc::new(Counters::new()),
                    stopping: Arc::new(AtomicBool::new(false)),
                    handle: Mutex::new(None),
                }
            }
        }
    }

    fn spawn_writer(
        path: &str,
    ) -> Result<
        (
            SyncSender<LedgerRow>,
            Arc<Counters>,
            Arc<AtomicBool>,
            std::thread::JoinHandle<()>,
        ),
        rusqlite::Error,
    > {
        // 建表在主线程做：失败要立刻知道（而不是在写线程里静默）
        let conn = Self::connect(path)?;
        Self::init_schema(&conn)?;
        drop(conn);

        let (tx, rx) = sync_channel::<LedgerRow>(QUEUE_CAP);
        let counters = Arc::new(Counters::new());
        let stopping = Arc::new(AtomicBool::new(false));
        let c = counters.clone();
        let st = stopping.clone();
        let p = path.to_string();

        let handle = std::thread::Builder::new()
            .name("nq-exec-store".into())
            .spawn(move || run_writer(&p, rx, c, st))
            .expect("spawn store writer");

        Ok((tx, counters, stopping, handle))
    }

    fn connect(path: &str) -> Result<Connection, rusqlite::Error> {
        let conn = Connection::open(path)?;
        // WAL + 适度 busy_timeout：与 Python 侧同口径，避免读写互卡
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(Duration::from_millis(3000))?;
        Ok(conn)
    }

    fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS exec_orders (
                client_order_id TEXT PRIMARY KEY,
                symbol          TEXT NOT NULL,
                state           TEXT NOT NULL,
                filled_qty      REAL NOT NULL DEFAULT 0,
                avg_price       REAL NOT NULL DEFAULT 0,
                updated_at      INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS exec_order_history (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                client_order_id TEXT NOT NULL,
                state           TEXT NOT NULL,
                ts              INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_exec_hist_cid ON exec_order_history(client_order_id);
            "#,
        )
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 投递一条台账（幂等 upsert）。队列满或未启用时静默返回 false。
    pub fn submit(&self, row: LedgerRow) -> bool {
        let guard = match self.tx.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(tx) = guard.as_ref() else {
            return false;
        };
        match tx.try_send(row) {
            Ok(()) => {
                self.counters.pending.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Full(_)) => {
                // 队列满：丢弃而不是阻塞下单路径
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut slot) = self.counters.last_error.lock() {
                    *slot = "写线程已退出".into();
                }
                false
            }
        }
    }

    pub fn status(&self) -> StoreStatus {
        StoreStatus {
            enabled: self.enabled,
            path: self.path.clone(),
            pending: self.counters.pending.load(Ordering::Relaxed) as usize,
            written: self.counters.written.load(Ordering::Relaxed),
            batches: self.counters.batches.load(Ordering::Relaxed),
            dropped: self.counters.dropped.load(Ordering::Relaxed),
            errors: self.counters.errors.load(Ordering::Relaxed),
            last_error: self.counters.last_error.lock().map(|s| s.clone()).unwrap_or_default(),
            last_batch_ms: self.counters.last_batch_ms.load(Ordering::Relaxed),
        }
    }

    /// 停止并 drain 队列（进程退出前调用）。可重复调用。
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        // 关闭发送端：写线程看到 Disconnected 后把剩余清空再退出
        if let Ok(mut g) = self.tx.lock() {
            *g = None;
        }
        if let Ok(mut g) = self.handle.lock() {
            if let Some(handle) = g.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // Arc 共享时最后一个引用析构才走到这里；shutdown 幂等（handle 已 take 为空）
        self.shutdown();
    }
}

/// 写线程主循环：批量取 + 单事务提交。
fn run_writer(path: &str, rx: Receiver<LedgerRow>, counters: Arc<Counters>, stopping: Arc<AtomicBool>) {
    let mut conn = match Store::connect(path).and_then(|c| {
        Store::init_schema(&c)?;
        Ok(c)
    }) {
        Ok(c) => c,
        Err(exc) => {
            counters.errors.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut slot) = counters.last_error.lock() {
                *slot = format!("写线程连库失败: {exc}");
            }
            return;
        }
    };

    loop {
        // 阻塞取第一条；断开且无数据则退出
        let first = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(row) => Some(row),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if stopping.load(Ordering::Relaxed) {
                    break;
                }
                None
            }
        };
        let Some(first) = first else {
            if stopping.load(Ordering::Relaxed) {
                // 再排空一次剩余
                let mut drained = Vec::new();
                while let Ok(r) = rx.try_recv() {
                    drained.push(r);
                }
                if !drained.is_empty() {
                    flush_batch(&mut conn, &drained, &counters);
                }
                break;
            }
            continue;
        };

        // 补齐一批
        let mut batch = vec![first];
        while batch.len() < BATCH_MAX {
            match rx.try_recv() {
                Ok(row) => batch.push(row),
                Err(_) => break,
            }
        }
        flush_batch(&mut conn, &batch, &counters);
        counters
            .pending
            .fetch_sub(batch.len() as u64, Ordering::Relaxed);
    }
}

fn flush_batch(conn: &mut Connection, batch: &[LedgerRow], counters: &Counters) {
    let started = Instant::now();
    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(exc) => {
            counters.errors.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut slot) = counters.last_error.lock() {
                *slot = format!("开启事务失败: {exc}");
            }
            return;
        }
    };
    let mut ok = 0u64;
    for row in batch {
        let upsert = tx.execute(
            r#"INSERT INTO exec_orders(client_order_id, symbol, state, filled_qty, avg_price, updated_at)
               VALUES(?1, ?2, ?3, ?4, ?5, ?6)
               ON CONFLICT(client_order_id) DO UPDATE SET
                 state=excluded.state,
                 filled_qty=excluded.filled_qty,
                 avg_price=excluded.avg_price,
                 updated_at=excluded.updated_at"#,
            params![
                row.client_order_id,
                row.symbol,
                row.state,
                row.filled_qty,
                row.avg_price,
                row.ts
            ],
        );
        let hist = tx.execute(
            "INSERT INTO exec_order_history(client_order_id, state, ts) VALUES(?1, ?2, ?3)",
            params![row.client_order_id, row.state, row.ts],
        );
        if upsert.is_ok() && hist.is_ok() {
            ok += 1;
        }
    }
    if let Err(exc) = tx.commit() {
        counters.errors.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut slot) = counters.last_error.lock() {
            *slot = format!("提交事务失败: {exc}");
        }
        return;
    }
    counters.written.fetch_add(ok, Ordering::Relaxed);
    counters.batches.fetch_add(1, Ordering::Relaxed);
    counters
        .last_batch_ms
        .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("nq-store-{}-{}", tag, std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("ledger.db").to_string_lossy().to_string()
    }

    fn row(cid: &str, state: &str) -> LedgerRow {
        LedgerRow {
            client_order_id: cid.into(),
            symbol: "BTCUSDT".into(),
            state: state.into(),
            filled_qty: 0.01,
            avg_price: 100.0,
            ts: 1_700_000_000,
        }
    }

    #[test]
    fn empty_path_disables_store() {
        let mut store = Store::open("");
        assert!(!store.enabled());
        assert!(!store.submit(row("a", "FILLED")));
        assert_eq!(store.status().written, 0);
        store.shutdown();
    }

    #[test]
    fn batch_persists_and_upserts() {
        let path = tmp_db("batch");
        let mut store = Store::open(&path);
        assert!(store.enabled());
        assert!(store.submit(row("c1", "ACKED")));
        assert!(store.submit(row("c1", "FILLED")));
        assert!(store.submit(row("c2", "FILLED")));
        // 等写线程批处理完
        for _ in 0..50 {
            if store.status().written >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        store.shutdown();

        let conn = Connection::open(&path).unwrap();
        let (state, n): (String, i64) = conn
            .query_row(
                "SELECT state, (SELECT COUNT(*) FROM exec_orders) FROM exec_orders WHERE client_order_id='c1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "FILLED", "同名订单必须 upsert 到最后状态");
        assert_eq!(n, 2, "只有两笔订单，不能因 upsert 变多");
        let hist: i64 = conn
            .query_row("SELECT COUNT(*) FROM exec_order_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hist, 3, "每次投递都留一条历史");
    }

    #[test]
    fn shutdown_drains_pending() {
        let path = tmp_db("drain");
        let mut store = Store::open(&path);
        for i in 0..10 {
            assert!(store.submit(row(&format!("c{i}"), "FILLED")));
        }
        store.shutdown(); // drop 前必须把所有积压写完
        let conn = Connection::open(&path).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM exec_orders", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 10, "shutdown 必须 drain 队列");
    }
}
