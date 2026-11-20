//! 迁移期临时工具：复制数据库副本并写入标的池，供 `ops final-verdict` 离线演练。
use rusqlite::Connection;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("用法: seed_drill_db <src_db> <dst_db> <SYMBOL[,SYMBOL...]>");
        std::process::exit(2);
    }
    let source = Connection::open(&args[1]).expect("open source db");
    source
        .execute("VACUUM INTO ?1", [&args[2]])
        .expect("vacuum into destination");
    drop(source);
    let conn = Connection::open(&args[2]).expect("open destination db");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();
    for symbol in args[3].split(',').map(str::trim).filter(|s| !s.is_empty()) {
        conn.execute(
            "INSERT OR REPLACE INTO trading_symbols(\
             symbol,name,tier,sl_atr_mult,enabled,max_leverage,precision,tick_sz,min_sz,\
             min_notional,ct_val,min_score,note,sort_order,created_at,updated_at) \
             VALUES(?1,?2,'core',1.5,1,20,3,'0.1','0.001',5.0,0.001,0.0,'',0,?3,?3)",
            rusqlite::params![symbol.to_uppercase(), symbol.to_uppercase(), now],
        )
        .expect("insert trading symbol");
        println!("seeded {}", symbol.to_uppercase());
    }
}
