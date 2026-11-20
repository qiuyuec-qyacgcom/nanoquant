use rusqlite::{params, Connection};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("用法: seed_e2e_equity <db>");
        std::process::exit(2);
    }
    let conn = Connection::open(&args[1]).expect("open db");
    conn.execute(
        "CREATE TABLE IF NOT EXISTS equity_points(id INTEGER PRIMARY KEY, ts INTEGER, mode TEXT, \
         wallet REAL, available REAL, unrealized REAL, margin_used REAL, positions INTEGER)",
        [],
    )
    .expect("ensure equity_points table");
    conn.execute("DELETE FROM equity_points WHERE mode='demo'", [])
        .expect("clear demo equity points");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();
    for (offset, wallet) in [(7200_i64, 1000.0_f64), (3600, 1005.0), (0, 1010.0)] {
        conn.execute(
            "INSERT INTO equity_points(ts,mode,wallet,available,unrealized,margin_used,positions) \
             VALUES(?1,'demo',?2,?2,0,0,0)",
            params![now - offset, wallet],
        )
        .expect("insert equity point");
    }
    println!("seeded equity_points=3");
}
