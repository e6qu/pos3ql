use std::process::ExitCode;

use pos3ql::config::{Config, FmtBytes};
use pos3ql::io::reactor::Reactor;
use pos3ql::mem;
use pos3ql::server::Server;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("pos3ql: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let config = load_config()?;
    let server_bytes = Reactor::budget_bytes(config.max_connections as usize + 1)
        + 128 // canned refusal message scratch
        + (config.max_connections as usize) * 8; // slot bookkeeping worst case
    let plan = config.memory_plan(server_bytes, pos3ql::sql::Engine::extra_budget_bytes(&config));

    println!("pos3ql starting");
    println!("  listen_addr  {}", config.listen_addr);
    println!("  data_dir     {}", config.data_dir);
    println!("{plan}");
    println!(
        "  disk cache   {:>12} (disk, not RAM)",
        FmtBytes(config.disk_cache_bytes)
    );

    if config.object_store_sim {
        return Err(
            "object_store = sim is the in-process simulator's mode (it allocates freely); \
             the server refuses it — use object_store = on with a real endpoint"
                .to_string(),
        );
    }

    let mut budget = mem::Budget::new(plan.total());
    let mut server =
        Server::new(&config, &mut budget).map_err(|e| format!("startup failed: {e}"))?;

    // The IANA zone-name catalog walks /usr/share/zoneinfo, which allocates —
    // it must happen on this side of the freeze. Zone files themselves load
    // on demand into fixed pools.
    // The TLS pool covers the object-store client and, when server TLS is on,
    // up to max_connections concurrent server-side sessions.
    let tls_budget = config.tls_pool_bytes
        + if config.tls_on {
            config.max_connections as usize * pos3ql::pg::tls::SERVER_SESSION_BYTES
        } else {
            0
        };
    mem::guard::set_tls_budget(tls_budget as u64);
    pos3ql::sql::tzif::init_catalog();
    pos3ql::sql::exec::init_record_shapes();

    mem::guard::freeze();
    println!(
        "startup complete: memory frozen ({} of {} budget drawn); accepting connections",
        FmtBytes(budget.used()),
        FmtBytes(budget.total()),
    );
    server.run().map_err(|e| {
        format!(
            "event loop failed: kind={:?} os_error={:?}",
            e.kind(),
            e.raw_os_error()
        )
    })
}

fn load_config() -> Result<Config, String> {
    let mut args = std::env::args().skip(1);
    let mut config_path: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let path = args
                    .next()
                    .ok_or_else(|| "--config requires a path".to_string())?;
                config_path = Some(path);
            }
            "--help" | "-h" => {
                println!("usage: pos3ql [--config <path>]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}' (see --help)")),
        }
    }
    match config_path {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read config '{path}': {e}"))?;
            Config::parse(&text).map_err(|e| format!("{path}: {e}"))
        }
        None => {
            eprintln!("pos3ql: no --config given, using development defaults");
            Ok(Config::default_dev())
        }
    }
}
