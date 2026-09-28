use std::process::ExitCode;

use pos3ql::config::{Config, FmtBytes};
use pos3ql::mem;
use pos3ql::server::Server;

/// Server construction traverses fixed-capacity catalog state before runtime
/// allocation freezes. Keep its stack an explicit startup resource instead of
/// inheriting an OS- and build-mode-dependent main-thread limit.
const SERVER_STARTUP_STACK_BYTES: usize = pos3ql::sql::exec::QUERY_STACK_BYTES;

fn main() -> ExitCode {
    let worker = std::thread::Builder::new()
        .name("pos3ql-server".to_string())
        .stack_size(SERVER_STARTUP_STACK_BYTES)
        .spawn(run);
    match worker {
        Ok(worker) => match worker.join() {
            Ok(Ok(())) => ExitCode::SUCCESS,
            Ok(Err(message)) => {
                eprintln!("pos3ql: {message}");
                ExitCode::FAILURE
            }
            Err(_) => {
                eprintln!("pos3ql: server thread panicked");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!("pos3ql: could not start server thread: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let (config, operation) = load_config()?;
    match operation {
        Operation::Serve => {}
        Operation::Backup(name) => {
            let lsn = pos3ql::operations::create_backup(&config, &name)?;
            println!("backup {name} created at LSN {lsn}");
            return Ok(());
        }
        Operation::Restore { name, target } => {
            let lsn = match target {
                RestoreTarget::Backup => pos3ql::operations::restore_backup(&config, &name),
                RestoreTarget::Lsn(value) => {
                    pos3ql::operations::restore_backup_to_lsn(&config, &name, &value)
                }
                RestoreTarget::Time(value) => {
                    pos3ql::operations::restore_backup_to_time(&config, &name, &value)
                }
            }?;
            println!("backup {name} restored at LSN {lsn}");
            return Ok(());
        }
        Operation::ExportBackup {
            name,
            destination_config,
        } => {
            let destination = load_config_file(&destination_config)?;
            let lsn = pos3ql::operations::export_backup(&config, &destination, &name)?;
            println!("backup {name} exported at LSN {lsn}");
            return Ok(());
        }
        Operation::DeleteBackup(name) => {
            if pos3ql::operations::delete_backup(&config, &name)? {
                println!("backup {name} deleted");
            } else {
                return Err(format!("backup '{name}' does not exist"));
            }
            return Ok(());
        }
    }
    let server_bytes = Server::budget_bytes(&config);
    let plan = config.memory_plan(
        server_bytes,
        pos3ql::sql::Engine::extra_budget_bytes(&config),
    );

    println!("pos3ql starting");
    println!("  listen_addr  {}", config.listen_addr);
    println!("  data_dir     {}", config.data_dir);
    println!("{plan}");
    println!(
        "  disk cache   {:>12} (disk, not RAM)",
        FmtBytes(config.disk_cache_bytes)
    );
    println!(
        "  temp spill   {:>12} (local ephemeral disk)",
        FmtBytes(config.temporary_spill_bytes)
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

    // The TLS pool covers the object-store client, every startup-bounded
    // outbound subscription worker, and, when server TLS is on, every
    // concurrent server-side session.
    let tls_budget = config.tls_pool_bytes
        + Server::extra_tls_pool_bytes(&config)
        + if config.tls_on {
            config.max_connections as usize * pos3ql::pg::tls::SERVER_SESSION_BYTES
        } else {
            0
        };
    mem::guard::set_tls_budget(tls_budget as u64);
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

enum Operation {
    Serve,
    Backup(String),
    Restore {
        name: String,
        target: RestoreTarget,
    },
    ExportBackup {
        name: String,
        destination_config: String,
    },
    DeleteBackup(String),
}

enum RestoreTarget {
    Backup,
    Lsn(String),
    Time(String),
}

fn load_config() -> Result<(Config, Operation), String> {
    let mut args = std::env::args().skip(1);
    let mut config_path: Option<String> = None;
    let mut destination_config_path: Option<String> = None;
    let mut recovery_target_lsn: Option<String> = None;
    let mut recovery_target_time: Option<String> = None;
    let mut operation = Operation::Serve;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let path = args
                    .next()
                    .ok_or_else(|| "--config requires a path".to_string())?;
                config_path = Some(path);
            }
            "--backup" | "--restore" | "--export-backup" | "--delete-backup" => {
                if !matches!(operation, Operation::Serve) {
                    return Err("only one backup operation may be requested".to_string());
                }
                let name = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a name"))?;
                operation = match arg.as_str() {
                    "--backup" => Operation::Backup(name),
                    "--restore" => Operation::Restore {
                        name,
                        target: RestoreTarget::Backup,
                    },
                    "--export-backup" => Operation::ExportBackup {
                        name,
                        destination_config: String::new(),
                    },
                    _ => Operation::DeleteBackup(name),
                };
            }
            "--destination-config" => {
                let path = args
                    .next()
                    .ok_or_else(|| "--destination-config requires a path".to_string())?;
                if destination_config_path.replace(path).is_some() {
                    return Err("--destination-config may be specified only once".to_string());
                }
            }
            "--recovery-target-lsn" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--recovery-target-lsn requires a pg_lsn".to_string())?;
                if recovery_target_lsn.replace(value).is_some() {
                    return Err("--recovery-target-lsn may be specified only once".to_string());
                }
            }
            "--recovery-target-time" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--recovery-target-time requires a timestamp".to_string())?;
                if recovery_target_time.replace(value).is_some() {
                    return Err("--recovery-target-time may be specified only once".to_string());
                }
            }
            "--help" | "-h" => {
                println!(
                    "usage: pos3ql [--config <path>] [--backup <name> | --restore <name> [--recovery-target-lsn <pg_lsn> | --recovery-target-time <timestamptz>] | --export-backup <name> --destination-config <path> | --delete-backup <name>]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}' (see --help)")),
        }
    }
    operation = match (operation, destination_config_path) {
        (
            Operation::ExportBackup {
                name,
                destination_config: _,
            },
            Some(destination_config),
        ) => Operation::ExportBackup {
            name,
            destination_config,
        },
        (Operation::ExportBackup { .. }, None) => {
            return Err("--export-backup requires --destination-config".to_string());
        }
        (_, Some(_)) => {
            return Err("--destination-config requires --export-backup".to_string());
        }
        (operation, None) => operation,
    };
    operation = match (operation, recovery_target_lsn, recovery_target_time) {
        (Operation::Restore { name, .. }, Some(value), None) => Operation::Restore {
            name,
            target: RestoreTarget::Lsn(value),
        },
        (Operation::Restore { name, .. }, None, Some(value)) => Operation::Restore {
            name,
            target: RestoreTarget::Time(value),
        },
        (Operation::Restore { .. }, Some(_), Some(_)) => {
            return Err("only one recovery target may be specified".to_string());
        }
        (_, Some(_), _) => {
            return Err("--recovery-target-lsn requires --restore".to_string());
        }
        (_, _, Some(_)) => {
            return Err("--recovery-target-time requires --restore".to_string());
        }
        (operation, None, None) => operation,
    };
    let config = match config_path {
        Some(path) => load_config_file(&path),
        None => {
            eprintln!("pos3ql: no --config given, using development defaults");
            Ok(Config::default_dev())
        }
    }?;
    Ok((config, operation))
}

fn load_config_file(path: &str) -> Result<Config, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read config '{path}': {error}"))?;
    Config::parse(&text).map_err(|error| format!("{path}: {error}"))
}
