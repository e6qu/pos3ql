//! Measure one empty embedded engine per process; run builds on CI runners.
use pos3ql::config::{Config, TransactionMode};
use pos3ql::mem::budget::Budget;
use pos3ql::sql::Engine;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.len() != 10 || arguments[0] != "--tables"
        || arguments[2] != "--table-rows" || arguments[4] != "--mode"
        || arguments[6] != "--data-dir" || arguments[8] != "--budget-limit-bytes"
    {
        return Err("usage: embedded-memory --tables N --table-rows N --mode mvcc|locking --data-dir NEW_DIRECTORY --budget-limit-bytes N".into());
    }
    let mut config = Config::default_dev();
    config.max_tables = arguments[1].parse()?;
    config.table_rows = arguments[3].parse()?;
    config.transaction_mode = match arguments[5].as_str() {
        "mvcc" => TransactionMode::Mvcc,
        "locking" => TransactionMode::Locking,
        _ => return Err("mode must be mvcc or locking".into()),
    };
    if config.max_tables == 0 || config.table_rows == 0 {
        return Err("tables and table-rows must be positive".into());
    }
    config.catalog_history_bytes = if config.transaction_mode == TransactionMode::Mvcc { 1 << 20 } else { 0 };
    config.max_connections = 1;
    config.query_workspace_slots = 1;
    config.memtable_bytes = 1 << 20;
    config.work_arena_bytes = 8 << 20;
    config.object_store_on = false;
    config.data_dir = arguments[7].clone();
    let limit: usize = arguments[9].parse()?;
    let plan = config.memory_plan(0, Engine::extra_budget_bytes(&config));
    let reserved = plan.memtable + plan.tables + plan.block_cache;
    if reserved > limit {
        return Err(format!("engine reservation {reserved} exceeds explicit measurement budget {limit}").into());
    }
    std::fs::create_dir(&config.data_dir)?;
    let mut budget = Budget::new(reserved);
    let started = std::time::Instant::now();
    let engine = Engine::new(&config, &mut budget)?;
    let elapsed = started.elapsed().as_micros();
    // getrusage reports bytes on macOS and KiB on Linux. Each invocation owns
    // one engine, so this peak does not include previous sample engines.
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the complete output on success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: the successful call initialized `usage`.
    let peak = unsafe { usage.assume_init() }.ru_maxrss;
    let peak_bytes = if cfg!(target_os = "macos") { peak as u64 } else { peak as u64 * 1024 };
    println!("{{\"os\":\"{}\",\"arch\":\"{}\",\"mode\":\"{}\",\"max_tables\":{},\"table_rows\":{},\"max_connections\":1,\"query_workspace_slots\":1,\"memtable_bytes\":{},\"work_arena_bytes\":{},\"catalog_history_bytes\":{},\"max_row_versions_per_row\":{},\"large_object_pages\":{},\"planned_engine_bytes\":{},\"drawn_engine_bytes\":{},\"startup_peak_rss_bytes\":{},\"startup_microseconds\":{}}}",
        std::env::consts::OS,std::env::consts::ARCH,arguments[5],config.max_tables,config.table_rows,
        config.memtable_bytes,config.work_arena_bytes,config.catalog_history_bytes,
        config.max_row_versions_per_row,config.large_object_pages,reserved,budget.used(),peak_bytes,elapsed);
    drop(engine);
    Ok(())
}
