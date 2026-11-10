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
                &self.guc,
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
        "--table-metadata-bytes",
        "--table-metadata-leases",
        "--statistics-bytes",
        "--statistics-leases",
        "--index-statistics-slots",
        "--execution-control-bytes",
        "--execution-control-leases",
    ];
    if arguments.len() != flags.len() * 2
        || flags
            .iter()
            .enumerate()
            .any(|(index, flag)| arguments[index * 2] != *flag)
    {
        return Err("usage: embedded-memory --tables N --row-overlay-slots N --row-work-bytes N --row-work-leases N --definition-image-slots N --mode mvcc|locking --row-history-slots N --workload empty|hot|even|moving|wide|replacement|statistics|statistics-wide --rows N --data-dir NEW_DIRECTORY --budget-limit-bytes N --table-metadata-bytes N --table-metadata-leases N --statistics-bytes N --statistics-leases N --index-statistics-slots N --execution-control-bytes N --execution-control-leases N".into());
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
    config.table_metadata_bytes = arguments[23].parse()?;
    config.table_metadata_leases = arguments[25].parse()?;
    config.statistics_bytes = arguments[27].parse()?;
    config.statistics_leases = arguments[29].parse()?;
    config.index_statistics_slots = arguments[31].parse()?;
    config.execution_control_bytes = arguments[33].parse()?;
    config.execution_control_leases = arguments[35].parse()?;
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
    if !matches!(
        workload,
        "empty"
            | "hot"
            | "even"
            | "moving"
            | "wide"
            | "replacement"
            | "statistics"
            | "statistics-wide"
    ) || (workload == "empty") != (rows == 0)
        || rows > config.row_overlay_slots
    {
        return Err("choose empty with zero rows, or a populated workload with positive rows fitting the global overlay".into());
    }
    config.catalog_history_bytes = if config.transaction_mode == TransactionMode::Mvcc {
        1 << 20
    } else {
        0
    };
    config.max_connections = 1;
    // One connection: a single spool lease bounds its queued result output.
    config.result_spool_bytes = 64 << 10;
    config.result_spool_leases = 1;
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
    let startup_memory = memory_usage()?;
    let mut session_budget = Budget::new(session_reserved);
    session_budget
        .observe_draws(|component, bytes| eprintln!("budget-draw\tsession\t{bytes}\t{component}"));
    let workload_started = std::time::Instant::now();
    if workload != "empty" {
        let mut session = Session::new(&config, &mut session_budget)?;
        let mut columns = String::from("id integer, payload text");
        if matches!(workload, "wide" | "statistics-wide") {
            for column in 2..64 {
                columns.push_str(&format!(", field_{column} integer"));
            }
        }
        for table in 0..config.max_tables {
            session.execute(
                &mut engine,
                &format!("CREATE TABLE measured_{table}({columns})"),
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
                session.execute(engine, &format!("INSERT INTO measured_{table}(id,payload) SELECT g,repeat('x',64) FROM generate_series({first},{}) g", first + count - 1), None)?;
            }
            Ok(())
        };
        if matches!(workload, "even" | "statistics" | "statistics-wide") {
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
        if workload == "replacement" {
            session.execute(
                &mut engine,
                "CREATE UNIQUE INDEX measured_id ON measured_0(id)",
                None,
            )?;
            session.execute(&mut engine, "BEGIN; ALTER TABLE measured_0 ADD COLUMN note text; SAVEPOINT original; ALTER TABLE measured_0 ADD COLUMN discarded integer; ROLLBACK TO original; COMMIT", None)?;
            session.execute(&mut engine, "DROP INDEX measured_id", None)?;
            session.execute(
                &mut engine,
                "CREATE UNIQUE INDEX measured_id ON measured_0(id)",
                None,
            )?;
        }
        if matches!(workload, "statistics" | "statistics-wide") {
            for table in 0..config.max_tables {
                session.execute(&mut engine, &format!("ANALYZE measured_{table}"), None)?;
            }
            if workload == "statistics-wide" {
                session.execute(
                    &mut engine,
                    "BEGIN; SAVEPOINT original_statistics; \
                     ALTER TABLE measured_0 ALTER COLUMN id TYPE bigint; \
                     ANALYZE measured_0(id); \
                     ROLLBACK TO original_statistics; COMMIT",
                    None,
                )?;
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
    let final_memory = memory_usage()?;
    let current_source = startup_memory
        .current_source
        .map_or("null".to_string(), |source| format!("\"{source}\""));
    let startup_current = startup_memory
        .current_bytes
        .map_or("null".to_string(), |bytes| bytes.to_string());
    let final_current = final_memory
        .current_bytes
        .map_or("null".to_string(), |bytes| bytes.to_string());
    println!(
        "{{\"rss_metric_version\":2,\"rss_unit\":\"bytes\",\"rss_peak_source\":\"{}\",\"rss_current_source\":{},\"getrusage_maxrss_source\":\"getrusage_rusage_self_ru_maxrss\",\"startup_current_rss_bytes\":{},\"final_current_rss_bytes\":{},\"startup_getrusage_maxrss_bytes\":{},\"final_getrusage_maxrss_bytes\":{},\"os\":\"{}\",\"arch\":\"{}\",\"mode\":\"{}\",\"workload\":\"{}\",\"rows\":{},\"max_tables\":{},\"row_overlay_slots\":{},\"row_work_bytes\":{},\"row_work_leases\":{},\"definition_image_slots\":{},\"table_metadata_bytes\":{},\"table_metadata_leases\":{},\"statistics_bytes\":{},\"statistics_leases\":{},\"index_statistics_slots\":{},\"execution_control_bytes\":{},\"execution_control_leases\":{},\"max_connections\":1,\"query_workspace_slots\":1,\"max_prepared_transactions\":0,\"max_subscriptions\":1,\"max_ddl_per_transaction\":16,\"max_catalog_versions_per_object\":2,\"memtable_bytes\":{},\"work_arena_bytes\":{},\"sql_arena_bytes\":{},\"catalog_history_bytes\":{},\"row_history_slots\":{},\"max_row_versions_per_row\":{},\"large_object_pages\":{},\"startup_stack_bytes\":{},\"planned_engine_bytes\":{},\"drawn_engine_bytes\":{},\"planned_session_ceiling_bytes\":{},\"drawn_session_bytes\":{},\"startup_peak_rss_bytes\":{},\"final_peak_rss_bytes\":{},\"startup_microseconds\":{},\"workload_microseconds\":{}}}",
        startup_memory.peak_source,
        current_source,
        startup_current,
        final_current,
        startup_memory.getrusage_peak_bytes,
        final_memory.getrusage_peak_bytes,
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
        config.table_metadata_bytes,
        config.table_metadata_leases,
        config.statistics_bytes,
        config.statistics_leases,
        config.index_statistics_slots,
        config.execution_control_bytes,
        config.execution_control_leases,
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
        startup_memory.peak_bytes,
        final_memory.peak_bytes,
        startup_microseconds,
        workload_microseconds
    );
    Ok(())
}

struct MemoryUsage {
    peak_bytes: u64,
    current_bytes: Option<u64>,
    getrusage_peak_bytes: u64,
    peak_source: &'static str,
    current_source: Option<&'static str>,
}

#[derive(Debug, PartialEq, Eq)]
struct ProcMemoryStatus {
    peak_bytes: u64,
    current_bytes: u64,
}

fn parse_proc_memory_status(input: &[u8]) -> Result<ProcMemoryStatus, &'static str> {
    let mut peak = None;
    let mut current = None;
    for line in input.split(|byte| *byte == b'\n') {
        let (destination, raw) = if let Some(value) = line.strip_prefix(b"VmHWM:") {
            (&mut peak, value)
        } else if let Some(value) = line.strip_prefix(b"VmRSS:") {
            (&mut current, value)
        } else {
            continue;
        };
        if destination.is_some() {
            return Err("duplicate VmHWM or VmRSS in /proc/self/status");
        }
        let text =
            core::str::from_utf8(raw).map_err(|_| "invalid RSS bytes in /proc/self/status")?;
        let mut fields = text.split_ascii_whitespace();
        let digits = fields
            .next()
            .ok_or("missing RSS count in /proc/self/status")?;
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("invalid RSS count in /proc/self/status");
        }
        let count = digits
            .parse::<u64>()
            .map_err(|_| "RSS count overflow in /proc/self/status")?;
        if fields.next() != Some("kB") || fields.next().is_some() {
            return Err("RSS unit must be kB in /proc/self/status");
        }
        *destination = Some(
            count
                .checked_mul(1024)
                .ok_or("RSS byte overflow in /proc/self/status")?,
        );
    }
    Ok(ProcMemoryStatus {
        peak_bytes: peak.ok_or("missing VmHWM in /proc/self/status")?,
        current_bytes: current.ok_or("missing VmRSS in /proc/self/status")?,
    })
}

fn read_proc_memory_status() -> Result<ProcMemoryStatus, std::io::Error> {
    use std::io::Read;
    let mut file = std::fs::File::open("/proc/self/status")?;
    let mut bytes = [0u8; 8192];
    let mut used = 0;
    loop {
        let read = match file.read(&mut bytes[used..]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            break;
        }
        used += read;
        if used == bytes.len() {
            let mut extra = [0u8; 1];
            let extra_read = loop {
                match file.read(&mut extra) {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    result => break result?,
                }
            };
            if extra_read != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "/proc/self/status exceeds the 8192-byte measurement buffer",
                ));
            }
            break;
        }
    }
    parse_proc_memory_status(&bytes[..used])
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))
}

fn memory_usage() -> Result<MemoryUsage, std::io::Error> {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
    }
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the complete output on success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful call initialized usage.
    let peak = u64::try_from(unsafe { usage.assume_init() }.ru_maxrss)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
    if cfg!(target_os = "linux") {
        // Linux exec preserves pre-exec high water in getrusage. VmHWM belongs
        // to the current address space, so launcher memory cannot set its floor.
        let current = read_proc_memory_status()?;
        Ok(MemoryUsage {
            peak_bytes: current.peak_bytes,
            current_bytes: Some(current.current_bytes),
            getrusage_peak_bytes: peak
                .checked_mul(1024)
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidData))?,
            peak_source: "linux_proc_status_vmhwm",
            current_source: Some("linux_proc_status_vmrss"),
        })
    } else {
        Ok(MemoryUsage {
            peak_bytes: peak,
            current_bytes: None,
            getrusage_peak_bytes: peak,
            peak_source: "darwin_getrusage_ru_maxrss",
            current_source: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_memory_fields_have_explicit_binary_kilobyte_units() {
        pos3ql::mem::guard::forbid_alloc(|| {
            assert_eq!(
                parse_proc_memory_status(b"Name:\tmeasurement\nVmHWM:\t1024 kB\nVmRSS: 512 kB\n")
                    .unwrap(),
                ProcMemoryStatus {
                    peak_bytes: 1_048_576,
                    current_bytes: 524_288
                }
            );
        });
    }

    #[test]
    fn proc_memory_fields_reject_missing_malformed_or_overflowed_values() {
        for input in [
            &b"VmHWM: 1 kB\n"[..],
            &b"VmRSS: 1 kB\n"[..],
            &b"VmHWM: 1 bytes\nVmRSS: 1 kB\n"[..],
            &b"VmHWM: 1 kB\nVmRSS: 1 KB\n"[..],
            &b"VmHWM: -1 kB\nVmRSS: 1 kB\n"[..],
            &b"VmHWM: +1 kB\nVmRSS: 1 kB\n"[..],
            &b"VmHWM: 1 kB extra\nVmRSS: 1 kB\n"[..],
            &b"VmHWM: 18446744073709551616 kB\nVmRSS: 1 kB\n"[..],
            &b"VmHWM: 18446744073709551615 kB\nVmRSS: 1 kB\n"[..],
            &b"VmHWM: 1 kB\nVmHWM: 2 kB\nVmRSS: 1 kB\n"[..],
            &b"VmHWM: 1 kB\nVmRSS: 1 kB\nVmRSS: 2 kB\n"[..],
        ] {
            pos3ql::mem::guard::forbid_alloc(|| assert!(parse_proc_memory_status(input).is_err()));
        }
    }

    #[test]
    fn memory_snapshot_identifies_the_platform_metric() {
        let usage = memory_usage().unwrap();
        if cfg!(target_os = "linux") {
            assert_eq!(usage.peak_source, "linux_proc_status_vmhwm");
            assert_eq!(usage.current_source, Some("linux_proc_status_vmrss"));
            assert!(usage.current_bytes.unwrap() > 0);
        } else {
            assert_eq!(usage.peak_source, "darwin_getrusage_ru_maxrss");
            assert_eq!(usage.current_bytes, None);
            assert_eq!(usage.current_source, None);
        }
        assert!(usage.peak_bytes > 0);
    }
}
