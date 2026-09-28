//! Offline durable operations over one configured object-store prefix.

use crate::config::Config;
use crate::mem::Budget;
use crate::sql::Engine;

fn operation_budget(config: &Config) -> Budget {
    Budget::new(operation_budget_bytes(config))
}

fn operation_budget_bytes(config: &Config) -> usize {
    let plan = config.memory_plan(
        crate::server::Server::budget_bytes(config),
        Engine::extra_budget_bytes(config),
    );
    plan.total()
}

fn require_object_store(config: &Config) -> Result<(), String> {
    if config.object_store_on {
        Ok(())
    } else {
        Err("durable backup operations require object_store = on".to_string())
    }
}

/// Checkpoints the recovered database and pins its object graph under `name`.
/// Run while no server process is using the configured prefix.
pub fn create_backup(config: &Config, name: &str) -> Result<u64, String> {
    require_object_store(config)?;
    let mut budget = operation_budget(config);
    let mut engine = Engine::new(config, &mut budget)
        .map_err(|error| format!("backup startup failed: {error}"))?;
    engine
        .create_backup(name)
        .map_err(|error| format!("backup failed: {}", error.message.as_str()))
}

/// Replaces the live manifest and commit head with a named backup and clears
/// the local journal and block cache. Run while no server process is active.
pub fn restore_backup(config: &Config, name: &str) -> Result<u64, String> {
    require_object_store(config)?;
    let mut budget = operation_budget(config);
    crate::checkpoint::restore_backup(config, &mut budget, name)
        .map_err(|error| format!("restore failed: {error}"))
}

/// Copies a named backup and its durable object graph into an empty destination
/// prefix, then publishes that recovery point as the destination's live state.
/// Run while no process is using either configured prefix.
pub fn export_backup(source: &Config, destination: &Config, name: &str) -> Result<u64, String> {
    require_object_store(source)?;
    require_object_store(destination)?;
    let mut budget = Budget::new(
        operation_budget_bytes(source).saturating_add(operation_budget_bytes(destination)),
    );
    crate::checkpoint::export_backup(source, destination, &mut budget, name)
        .map_err(|error| format!("backup export failed: {error}"))
}

/// Removes a named backup's retention pin. Run while no server process is
/// using the configured prefix.
pub fn delete_backup(config: &Config, name: &str) -> Result<bool, String> {
    require_object_store(config)?;
    let mut budget = operation_budget(config);
    let mut engine = Engine::new(config, &mut budget)
        .map_err(|error| format!("backup deletion startup failed: {error}"))?;
    engine
        .delete_backup(name)
        .map_err(|error| format!("backup deletion failed: {}", error.message.as_str()))
}
