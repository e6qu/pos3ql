//! Measure one embedded engine per process; run the matrix on CI runners.
use pos3ql::config::{Config, TransactionMode};
use pos3ql::mem::{Arena, FixedBuf, budget::Budget};
use pos3ql::pg::respond::Responder;
use pos3ql::sql::{
    Engine, ExecutionStatus, cursor::CursorPool, guc::GucState, prep::SqlPreparedPool,
    txn::TxnState,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::thread::Builder::new()
        .name("embedded-memory".to_string())
        .stack_size(pos3ql::sql::exec::QUERY_STACK_BYTES)
        .spawn(|| measure().map_err(|error| error.to_string()))?
        .join()
        .map_err(|_| "embedded measurement worker panicked")??;
    Ok(())
}

struct Session {
    send: FixedBuf,
    arena: Arena,
    transaction: TxnState,
    guc: GucState,
    prepared: SqlPreparedPool,
    cursors: CursorPool,
}

impl Session {
    fn new(config: &Config, budget: &mut Budget) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            send: FixedBuf::new(
                budget,
                "measurement response",
                config.conn_send_buffer_bytes,
            )?,
            arena: Arena::new(budget, "measurement statement", config.sql_arena_bytes)?,
            transaction: TxnState::new_with_config(budget, config)?,
            guc: GucState::new_with_capacities(
                budget,
                config.max_sequences,
                config.max_savepoints_per_transaction,
                config.transaction_mode,
            )?,
            prepared: SqlPreparedPool::new(config, budget)?,
            cursors: CursorPool::new(config, budget)?,
        })
    }

    fn execute(
        &mut self,
        engine: &mut Engine,
        sql: &str,
        expected: Option<(u64, i64)>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.send.clear();
        self.arena.reset();
        let status = pos3ql::mem::guard::forbid_alloc(|| {
            engine.execute_simple(
                sql,
                &self.arena,
                &mut self.transaction,
                &mut self.prepared,
                &mut self.cursors,
                &mut self.guc,
                &mut Responder::new(&mut self.send),
                1,
            )
        });
        let status = status.map_err(|_| "measurement response buffer exhausted")?;
        if status != ExecutionStatus::Complete {
            return Err(format!("local measurement unexpectedly suspended: {status:?}").into());
        }
        let mut messages = self.send.readable();
        let mut actual = None;
        while !messages.is_empty() {
            if messages.len() < 5 {
                return Err("truncated measurement response".into());
            }
            let length = usize::try_from(i32::from_be_bytes(messages[1..5].try_into()?))?;
            if length < 4 || length + 1 > messages.len() {
                return Err("invalid measurement response length".into());
            }
            let payload = &messages[5..length + 1];
            if messages[0] == b'E' {
                return Err(format!(
                    "measurement SQL failed: {sql}: {}",
                    String::from_utf8_lossy(payload)
                )
                .into());
            }
            if messages[0] == b'D' {
                if actual.is_some() || payload.len() < 2 || payload[..2] != [0, 2] {
                    return Err("unexpected measurement result shape".into());
                }
                let mut fields = &payload[2..];
                let mut next_field = || -> Result<&str, Box<dyn std::error::Error>> {
                    if fields.len() < 4 {
                        return Err("truncated measurement result field".into());
                    }
                    let bytes = usize::try_from(i32::from_be_bytes(fields[..4].try_into()?))?;
                    if bytes > fields.len() - 4 {
                        return Err("invalid measurement result field length".into());
                    }
                    let source = fields;
                    let text = std::str::from_utf8(&source[4..4 + bytes])?;
                    fields = &source[4 + bytes..];
                    Ok(text)
                };
                let count = next_field()?.parse()?;
                let sum = next_field()?.parse()?;
                if !fields.is_empty() {
                    return Err("trailing measurement result fields".into());
                }
                actual = Some((count, sum));
            }
            messages = &messages[length + 1..];
        }
        if actual != expected {
            return Err(format!(
                "measurement result mismatch: {sql}: expected {expected:?}, got {actual:?}"
            )
            .into());
        }
        if self.transaction.is_active() {
            return Err("measurement left a transaction open".into());
        }
        Ok(())
    }
}

fn measure() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let flags = [
        "--tables",
        "--row-overlay-slots",
        "--row-work-bytes",
        "--row-work-leases",
        "--definition-image-slots",
        "--mode",
        "--row-history-slots",
        "--workload",
        "--rows",
        "--data-dir",
        "--budget-limit-bytes",
    ];
    if arguments.len() != flags.len() * 2
        || flags
            .iter()
            .enumerate()
            .any(|(index, flag)| arguments[index * 2] != *flag)
    {
        return Err("usage: embedded-memory --tables N --row-overlay-slots N --row-work-bytes N --row-work-leases N --definition-image-slots N --mode mvcc|locking --row-history-slots N --workload empty|hot|even|moving --rows N --data-dir NEW_DIRECTORY --budget-limit-bytes N".into());
    }
    let mut config = Config::default_dev();
    config.max_tables = arguments[1].parse()?;
    config.max_extended_statistics = 256;
    config.row_overlay_slots = arguments[3].parse()?;
    config.row_work_bytes = arguments[5].parse()?;
    config.row_work_leases = arguments[7].parse()?;
    config.definition_image_slots = arguments[9].parse()?;
    config.transaction_mode = match arguments[11].as_str() {
        "mvcc" => TransactionMode::Mvcc,
        "locking" => TransactionMode::Locking,
        _ => return Err("mode must be mvcc or locking".into()),
    };
    config.row_history_slots = arguments[13].parse()?;
    config.validate_runtime_resources()?;
    if config.max_tables == 0 {
        return Err("tables must be positive".into());
    }
    if (config.transaction_mode == TransactionMode::Mvcc) != (config.row_history_slots > 0) {
        return Err(
            "row-history-slots must be positive in mvcc mode and zero in locking mode".into(),
        );
    }
    let workload = arguments[15].as_str();
    let rows: usize = arguments[17].parse()?;
    if !matches!(workload, "empty" | "hot" | "even" | "moving")
        || (workload == "empty") != (rows == 0)
        || rows > config.row_overlay_slots
    {
        return Err("choose empty with zero rows, or hot/even/moving with positive rows fitting the global overlay".into());
    }
    config.catalog_history_bytes = if config.transaction_mode == TransactionMode::Mvcc {
        1 << 20
    } else {
        0
    };
    config.max_connections = 1;
    config.query_workspace_slots = 1;
    config.max_prepared_transactions = 0;
    config.max_subscriptions = 1;
    config.max_ddl_per_transaction = 16;
    config.max_catalog_versions_per_object = 2;
    config.memtable_bytes = 1 << 20;
    config.sql_arena_bytes = 8 << 20;
    config.work_arena_bytes = 8 << 20;
    config.object_store_on = false;
    config.temporary_spill_bytes = 0;
    config.data_dir = arguments[19].clone();
    let limit: usize = arguments[21].parse()?;
    let plan = config.memory_plan(0, Engine::extra_budget_bytes(&config));
    let reserved = plan
        .memtable
        .saturating_add(plan.tables)
        .saturating_add(plan.block_cache);
    let session_reserved = if workload == "empty" {
        0
    } else {
        plan.connections
    };
    if reserved
        .checked_add(session_reserved)
        .ok_or("measurement reservation overflow")?
        > limit
    {
        return Err(format!("engine {reserved} plus session {session_reserved} exceeds explicit measurement budget {limit}").into());
    }
    std::fs::create_dir(&config.data_dir)?;
    let mut budget = Budget::new(reserved);
    budget.observe_draws(|component, bytes| eprintln!("budget-draw\tengine\t{bytes}\t{component}"));
    let started = std::time::Instant::now();
    let mut engine = Engine::new(&config, &mut budget)?;
    let startup_microseconds = started.elapsed().as_micros();
    let startup_peak_rss_bytes = peak_rss_bytes()?;
    let mut session_budget = Budget::new(session_reserved);
    session_budget
        .observe_draws(|component, bytes| eprintln!("budget-draw\tsession\t{bytes}\t{component}"));
    let workload_started = std::time::Instant::now();
    if workload != "empty" {
        let mut session = Session::new(&config, &mut session_budget)?;
        for table in 0..config.max_tables {
            session.execute(
                &mut engine,
                &format!("CREATE TABLE measured_{table}(id integer, payload text)"),
                None,
            )?;
        }
        let mut counts = vec![0u64; config.max_tables];
        let mut sums = vec![0i64; config.max_tables];
        let populate = |session: &mut Session,
                        engine: &mut Engine,
                        table: usize,
                        first: usize,
                        count: usize|
         -> Result<(), Box<dyn std::error::Error>> {
            if count != 0 {
                session.execute(engine, &format!("INSERT INTO measured_{table} SELECT g,repeat('x',64) FROM generate_series({first},{}) g", first + count - 1), None)?;
            }
            Ok(())
        };
        if workload == "even" {
            let mut first = 0;
            for (table, (expected_count, expected_sum)) in
                counts.iter_mut().zip(&mut sums).enumerate()
            {
                let count =
                    rows / config.max_tables + usize::from(table < rows % config.max_tables);
                populate(&mut session, &mut engine, table, first, count)?;
                *expected_count = count as u64;
                *expected_sum = (first..first + count).map(|id| id as i64).sum();
                first += count;
            }
        } else {
            populate(&mut session, &mut engine, 0, 0, rows)?;
            counts[0] = rows as u64;
            sums[0] = (0..rows).map(|id| id as i64).sum();
            if workload == "moving" {
                let mut prior = 0;
                for target in [config.max_tables - 1, config.max_tables / 2, 0] {
                    session.execute(&mut engine, &format!("TRUNCATE measured_{prior}"), None)?;
                    populate(&mut session, &mut engine, target, 0, rows)?;
                    counts[target] = rows as u64;
                    sums[target] = (0..rows).map(|id| id as i64).sum();
                    // Clear the old table's expected answer after its lease moves.
                    if prior != target {
                        counts[prior] = 0;
                        sums[prior] = 0;
                    }
                    prior = target;
                }
            }
        }
        for (table, (&count, &sum)) in counts.iter().zip(&sums).enumerate() {
            session.execute(
                &mut engine,
                &format!("SELECT count(*),coalesce(sum(id),0) FROM measured_{table}"),
                Some((count, sum)),
            )?;
        }
    }
    let workload_microseconds = workload_started.elapsed().as_micros();
    let final_peak_rss_bytes = peak_rss_bytes()?;
    println!(
        "{{\"os\":\"{}\",\"arch\":\"{}\",\"mode\":\"{}\",\"workload\":\"{}\",\"rows\":{},\"max_tables\":{},\"row_overlay_slots\":{},\"row_work_bytes\":{},\"row_work_leases\":{},\"definition_image_slots\":{},\"max_connections\":1,\"query_workspace_slots\":1,\"max_prepared_transactions\":0,\"max_subscriptions\":1,\"max_ddl_per_transaction\":16,\"max_catalog_versions_per_object\":2,\"memtable_bytes\":{},\"work_arena_bytes\":{},\"sql_arena_bytes\":{},\"catalog_history_bytes\":{},\"row_history_slots\":{},\"max_row_versions_per_row\":{},\"large_object_pages\":{},\"startup_stack_bytes\":{},\"planned_engine_bytes\":{},\"drawn_engine_bytes\":{},\"planned_session_ceiling_bytes\":{},\"drawn_session_bytes\":{},\"startup_peak_rss_bytes\":{},\"final_peak_rss_bytes\":{},\"startup_microseconds\":{},\"workload_microseconds\":{}}}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        arguments[11],
        workload,
        rows,
        config.max_tables,
        config.row_overlay_slots,
        config.row_work_bytes,
        config.row_work_leases,
        config.definition_image_slots,
        config.memtable_bytes,
        config.work_arena_bytes,
        config.sql_arena_bytes,
        config.catalog_history_bytes,
        config.row_history_slots,
        config.max_row_versions_per_row,
        config.large_object_pages,
        pos3ql::sql::exec::QUERY_STACK_BYTES,
        reserved,
        budget.used(),
        session_reserved,
        session_budget.used(),
        startup_peak_rss_bytes,
        final_peak_rss_bytes,
        startup_microseconds,
        workload_microseconds
    );
    Ok(())
}

fn peak_rss_bytes() -> Result<u64, std::io::Error> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the complete output on success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful call initialized usage.
    let peak = unsafe { usage.assume_init() }.ru_maxrss as u64;
    Ok(if cfg!(target_os = "macos") {
        peak
    } else {
        peak * 1024
    })
}
