//! Engine tests: statements driven end to end against a temporary instance.
//!
//! These exercise `Engine` through the same entry points a connection uses, so
//! they cover parsing, execution, transactions and the wire encoding together
//! rather than any one of them alone.

use super::*;

fn test_config(name: &str) -> Config {
    let dir = std::env::temp_dir().join(format!("pos3ql-engine-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    let mut config = Config::default_dev();
    config.data_dir = dir.to_str().unwrap().to_string();
    config.memtable_bytes = 1 << 20;
    config.max_connections = 8;
    config.max_tables = 8;
    config.table_rows = 1024;
    config.txn_rows = 2048;
    config.value_index_rows = 2048;
    config.max_value_indexes = 8;
    config.wal_bytes = 1 << 20;
    config.wal_buffer_bytes = 1 << 14;
    config.work_arena_bytes = 1 << 21;
    config
}

fn test_engine() -> (Engine, Budget) {
    // Each test gets its own journal; the caller's function name is not
    // available, so a counter differentiates them.
    use core::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let name = format!("t{n}");
    let config = test_config(&name);
    let mut budget = Budget::new(1 << 26);
    let engine = Engine::new(&config, &mut budget).unwrap();
    (engine, budget)
}

fn run_with(engine: &mut Engine, budget: &mut Budget, sql_text: &str) -> Vec<u8> {
    let mut guc = GucState::new();
    run_with_guc(engine, budget, sql_text, 1 << 18, &mut guc)
}

fn run_with_arena_bytes(
    engine: &mut Engine,
    budget: &mut Budget,
    sql_text: &str,
    arena_bytes: usize,
) -> Vec<u8> {
    let mut guc = GucState::new();
    run_with_guc(engine, budget, sql_text, arena_bytes, &mut guc)
}

fn run_with_guc(
    engine: &mut Engine,
    budget: &mut Budget,
    sql_text: &str,
    arena_bytes: usize,
    guc: &mut GucState,
) -> Vec<u8> {
    let mut buffer = crate::mem::FixedBuf::new(budget, "send", 1 << 18).unwrap();
    let arena = Arena::new(budget, "sql", arena_bytes).unwrap();
    let mut txn = TxnState::new(budget, 1024).unwrap();
    let mut pool = test_pool(budget);
    let mut responder = Responder::new(&mut buffer);
    engine
        .execute_simple(
            sql_text,
            &arena,
            &mut txn,
            &mut pool,
            &mut test_cursors(budget),
            guc,
            &mut responder,
            1,
        )
        .unwrap();
    buffer.readable().to_vec()
}

/// Runs one simple-query message as a specific connection id (for LISTEN /
/// NOTIFY, whose semantics are cross-connection).
fn run_as(engine: &mut Engine, budget: &mut Budget, conn_id: i32, sql_text: &str) -> Vec<u8> {
    let mut buffer = crate::mem::FixedBuf::new(budget, "send", 1 << 18).unwrap();
    let arena = Arena::new(budget, "sql", 1 << 18).unwrap();
    let mut txn = TxnState::new(budget, 1024).unwrap();
    let mut pool = test_pool(budget);
    let mut guc = GucState::new();
    let mut responder = Responder::new(&mut buffer);
    engine
        .execute_simple(
            sql_text,
            &arena,
            &mut txn,
            &mut pool,
            &mut test_cursors(budget),
            &mut guc,
            &mut responder,
            conn_id,
        )
        .unwrap();
    buffer.readable().to_vec()
}

fn test_pool(budget: &mut Budget) -> SqlPreparedPool {
    let mut c = Config::default_dev();
    c.max_prepared = 4;
    c.prepared_bytes = 1024;
    SqlPreparedPool::new(&c, budget).unwrap()
}

fn test_cursors(budget: &mut Budget) -> crate::sql::cursor::CursorPool {
    let mut c = Config::default_dev();
    c.max_cursors = 2;
    c.cursor_bytes = 16 * 1024;
    crate::sql::cursor::CursorPool::new(&c, budget).unwrap()
}

#[test]
fn role_catalog_is_transactional_and_attribute_complete() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE analyst LOGIN CREATEDB NOINHERIT CONNECTION LIMIT 3 PASSWORD 'secret'
             VALID UNTIL '2030-01-02 03:04:05+00';
         SELECT rolname, rolsuper, rolinherit, rolcreatedb, rolcanlogin,
                rolconnlimit, rolvaliduntil IS NOT NULL, rolreplication, rolbypassrls
           FROM pg_roles WHERE rolname = 'analyst';
         ALTER USER analyst WITH NOLOGIN REPLICATION BYPASSRLS;
         SELECT rolcanlogin, rolreplication, rolbypassrls
           FROM pg_roles WHERE rolname = 'analyst';
         SET ROLE analyst;
         SELECT current_user, session_user, current_role;
         RESET ROLE;
         SELECT current_user, session_user, current_role;",
    );
    assert_eq!(
        data_rows(&output),
        [
            "analyst|f|f|t|t|3|t|f|f",
            "f|t|t",
            "analyst|postgres|analyst",
            "postgres|postgres|postgres",
        ],
        "{}",
        String::from_utf8_lossy(&output)
    );

    let output = run_with(
        &mut engine,
        &mut budget,
        "BEGIN;
         CREATE GROUP transient;
         SELECT count(*) FROM pg_roles WHERE rolname = 'transient';
         ROLLBACK;
         SELECT count(*) FROM pg_roles WHERE rolname = 'transient';",
    );
    assert_eq!(data_rows(&output), ["1", "0"]);

    let output = run_with(
        &mut engine,
        &mut budget,
        "DROP ROLE analyst; SELECT count(*) FROM pg_roles WHERE rolname = 'analyst'",
    );
    assert_eq!(
        data_rows(&output),
        ["0"],
        "{}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn role_connection_limit_is_reserved_and_released_exactly() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE limited_login LOGIN CONNECTION LIMIT 1",
    );
    assert!(!String::from_utf8_lossy(&output).contains("ERROR"));
    let login = engine.role_login("limited_login").unwrap();
    assert!(login.can_login && login.valid);
    assert!(engine.reserve_role_connection(login));
    assert!(!engine.reserve_role_connection(login));
    engine.release_role_connection(login.slot);
    assert!(engine.reserve_role_connection(login));
    engine.release_role_connection(login.slot);
}

#[test]
fn idle_connection_shutdown_does_not_claim_zero_wal_stage() {
    let (mut engine, mut budget) = test_engine();
    let mut transaction = TxnState::new(&mut budget, 8).unwrap();
    let guc = GucState::new();
    assert_eq!(transaction.txid, 0);
    engine.rollback_txn(&mut transaction, &guc);
    assert_eq!(transaction.txid, 0);
}

#[test]
fn create_role_authority_cannot_escalate_attributes_or_alter_unmanaged_roles() {
    let (mut engine, mut budget) = test_engine();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE role_administrator LOGIN CREATEROLE;
         CREATE ROLE managed_role;
         CREATE ROLE unmanaged_role;
         GRANT managed_role TO role_administrator WITH ADMIN OPTION;",
    );
    assert!(!String::from_utf8_lossy(&setup).contains("ERROR"));
    let allowed = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE role_administrator;
         ALTER ROLE managed_role LOGIN;
         ALTER ROLE role_administrator PASSWORD 'changed';
         CREATE ROLE ordinary_child;",
    );
    assert!(
        !String::from_utf8_lossy(&allowed).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&allowed)
    );
    let escalation = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE role_administrator; CREATE ROLE escalated SUPERUSER",
    );
    assert!(
        String::from_utf8_lossy(&escalation).contains("must be superuser"),
        "{}",
        String::from_utf8_lossy(&escalation)
    );
    let unmanaged = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE role_administrator; ALTER ROLE unmanaged_role LOGIN",
    );
    assert!(
        String::from_utf8_lossy(&unmanaged).contains("permission denied to alter role"),
        "{}",
        String::from_utf8_lossy(&unmanaged)
    );
}

#[test]
fn role_catalog_replays_from_wal() {
    let config = test_config("role-wal-replay");
    let mut budget = Budget::new(1 << 27);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE durable LOGIN CREATEROLE CONNECTION LIMIT 7 PASSWORD 'never-store-this';
         ALTER ROLE durable NOINHERIT CREATEDB;
         CREATE ROLE durable_member;
         GRANT durable TO durable_member WITH ADMIN OPTION;",
    );
    assert!(!output.is_empty());
    drop(engine);
    let wal_bytes =
        std::fs::read(std::path::Path::new(&config.data_dir).join("journal.wal")).unwrap();
    assert!(
        !wal_bytes
            .windows(b"never-store-this".len())
            .any(|window| window == b"never-store-this"),
        "role password leaked into WAL"
    );

    let mut restarted_budget = Budget::new(1 << 27);
    let mut restarted = Engine::new(&config, &mut restarted_budget).unwrap();
    let output = run_with(
        &mut restarted,
        &mut restarted_budget,
        "SELECT rolname, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin, rolconnlimit
           FROM pg_roles WHERE rolname = 'durable';
         SELECT parent.rolname, child.rolname, membership.admin_option
           FROM pg_auth_members membership
           JOIN pg_roles parent ON parent.oid = membership.roleid
           JOIN pg_roles child ON child.oid = membership.member",
    );
    assert_eq!(
        data_rows(&output),
        ["durable|f|t|t|t|7", "durable|durable_member|t"],
        "{}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn object_ownership_and_acl_enforce_and_replay() {
    let config = test_config("object-acl-wal-replay");
    let mut budget = Budget::new(1 << 27);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE app_owner;
         CREATE ROLE app_reader;
         GRANT CREATE ON SCHEMA public TO app_owner;
         SET ROLE app_owner;
         CREATE TABLE secured (id int PRIMARY KEY, value text);
         INSERT INTO secured VALUES (1, 'visible');
         RESET ROLE;
         GRANT SELECT ON TABLE secured TO app_reader;",
    );
    assert!(
        !String::from_utf8_lossy(&output).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&output)
    );

    let output = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE app_reader; SELECT value FROM secured",
    );
    assert_eq!(data_rows(&output), ["visible"]);

    let output = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE app_reader; INSERT INTO secured VALUES (2, 'hidden')",
    );
    assert!(
        String::from_utf8_lossy(&output).contains("permission denied for table secured"),
        "{}",
        String::from_utf8_lossy(&output)
    );

    let output = run_with(
        &mut engine,
        &mut budget,
        "ALTER TABLE secured OWNER TO app_reader;
         SELECT tableowner FROM pg_tables WHERE tablename = 'secured';",
    );
    assert_eq!(data_rows(&output), ["app_reader"]);
    drop(engine);

    let mut restarted_budget = Budget::new(1 << 27);
    let mut restarted = Engine::new(&config, &mut restarted_budget).unwrap();
    let output = run_with(
        &mut restarted,
        &mut restarted_budget,
        "SET ROLE app_reader;
         SELECT value FROM secured;
         INSERT INTO secured VALUES (2, 'owned');
         SELECT count(*) FROM secured;",
    );
    assert_eq!(data_rows(&output), ["visible", "2"]);
}

#[test]
fn role_membership_controls_set_role_and_catalog_rows() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE parent;
         CREATE ROLE child;
         GRANT parent TO child WITH ADMIN OPTION, INHERIT FALSE, SET TRUE;
         SELECT parent.rolname, child.rolname, membership.admin_option,
                membership.inherit_option, membership.set_option
           FROM pg_auth_members membership
           JOIN pg_roles parent ON parent.oid = membership.roleid
           JOIN pg_roles child ON child.oid = membership.member;
         SET ROLE child;
         SET ROLE parent;
         SELECT current_user, session_user;
         RESET ROLE;
         REVOKE ADMIN OPTION FOR parent FROM child;
         SELECT admin_option, inherit_option, set_option FROM pg_auth_members;
         REVOKE parent FROM child;
         SELECT count(*) FROM pg_auth_members;",
    );
    assert_eq!(
        data_rows(&output),
        ["parent|child|t|f|t", "parent|postgres", "f|f|t", "0",],
        "{}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn create_role_membership_clauses_match_grant_role_state() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE parent;
         CREATE ROLE ordinary_member;
         CREATE ROLE administrative_member;
         CREATE ROLE bundle IN ROLE parent ROLE ordinary_member ADMIN administrative_member;
         SELECT parent.rolname, child.rolname, membership.admin_option
           FROM pg_auth_members membership
           JOIN pg_roles parent ON parent.oid = membership.roleid
           JOIN pg_roles child ON child.oid = membership.member
          ORDER BY parent.rolname, child.rolname;",
    );
    assert_eq!(
        data_rows(&output),
        [
            "bundle|administrative_member|t",
            "bundle|ordinary_member|f",
            "parent|bundle|f",
        ],
        "{}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn role_rename_view_owner_and_privilege_inquiry_are_enforced() {
    let config = test_config("role-view-acl-replay");
    let mut budget = Budget::new(1 << 27);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE provisional;
         ALTER ROLE provisional RENAME TO app_owner;
         CREATE ROLE app_reader;
         GRANT CREATE ON SCHEMA public TO app_owner;
         SET ROLE app_owner;
         CREATE TABLE private_rows (id int, value text);
         INSERT INTO private_rows VALUES (1, 'through-view');
         CREATE VIEW exposed_rows AS SELECT value FROM private_rows;
         CREATE SEQUENCE exposed_sequence;
         RESET ROLE;
         GRANT SELECT ON exposed_rows TO app_reader;
         GRANT USAGE ON SEQUENCE exposed_sequence TO app_reader;
         SELECT oid, pg_get_userbyid(oid) FROM pg_roles WHERE rolname = 'app_owner';
         SELECT has_table_privilege('app_reader', 'exposed_rows', 'SELECT'),
                has_table_privilege('app_reader', 'private_rows', 'SELECT'),
                has_sequence_privilege('app_reader', 'exposed_sequence', 'USAGE'),
                has_schema_privilege('app_reader', 'public', 'USAGE');",
    );
    assert_eq!(
        data_rows(&output),
        ["16385|app_owner", "t|f|t|t"],
        "{}",
        String::from_utf8_lossy(&output)
    );

    let output = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE app_reader;
         SELECT value FROM exposed_rows;
         SELECT nextval('exposed_sequence');",
    );
    assert_eq!(
        data_rows(&output),
        ["through-view", "1"],
        "{}",
        String::from_utf8_lossy(&output)
    );
    let denied = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE app_reader; SELECT setval('exposed_sequence', 20)",
    );
    assert!(
        String::from_utf8_lossy(&denied).contains("permission denied for sequence"),
        "{}",
        String::from_utf8_lossy(&denied)
    );
    let dependent = run_with(&mut engine, &mut budget, "DROP ROLE app_owner");
    assert!(
        String::from_utf8_lossy(&dependent).contains("some objects depend on it"),
        "{}",
        String::from_utf8_lossy(&dependent)
    );
    drop(engine);

    let mut restarted_budget = Budget::new(1 << 27);
    let mut restarted = Engine::new(&config, &mut restarted_budget).unwrap();
    let output = run_with(
        &mut restarted,
        &mut restarted_budget,
        "SET ROLE app_reader;
         SELECT value FROM exposed_rows;
         SELECT has_table_privilege('app_reader', 'exposed_rows', 'SELECT');",
    );
    assert_eq!(
        data_rows(&output),
        ["through-view", "t"],
        "{}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn role_ownership_and_acl_survive_cold_object_store_recovery() {
    use core::sync::atomic::{AtomicU32, Ordering};

    static NEXT_BUCKET: AtomicU32 = AtomicU32::new(0);
    let sequence = NEXT_BUCKET.fetch_add(1, Ordering::SeqCst);
    let mut config = test_config(&format!("role-acl-cold-{sequence}"));
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("sql-role-acl-{}-{sequence}", std::process::id());
    config.object_store_response_bytes = 1 << 20;
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.wal_upload_buffer_bytes = 256 * 1024;
    config.block_cache_bytes = crate::store::BLOCK_SIZE;
    config.disk_cache_bytes = crate::store::BLOCK_SIZE;
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);

    let mut budget = Budget::new(1 << 28);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE durable_owner;
         CREATE ROLE durable_reader IN ROLE durable_owner;
         CREATE ROLE unprivileged;
         REVOKE USAGE ON SCHEMA public FROM PUBLIC;
         GRANT USAGE, CREATE ON SCHEMA public TO durable_owner;
         GRANT USAGE ON SCHEMA public TO durable_reader;
         ALTER DEFAULT PRIVILEGES FOR ROLE durable_owner
           GRANT SELECT ON TABLES TO durable_reader;
         SET ROLE durable_owner;
         CREATE TABLE durable_private (id int, value text);
         INSERT INTO durable_private VALUES (1, 'object-authority');
         CREATE VIEW durable_exposed AS SELECT value FROM durable_private;
         CREATE SEQUENCE durable_sequence;
         CREATE TYPE durable_state AS ENUM ('ready', 'blocked');
         RESET ROLE;
         REVOKE USAGE ON TYPE durable_state FROM PUBLIC;
         GRANT SELECT ON durable_exposed TO durable_reader;
         GRANT USAGE ON SEQUENCE durable_sequence TO durable_reader;",
    );
    assert!(
        !String::from_utf8_lossy(&output).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert!(engine.checkpoint().unwrap());
    drop(engine);

    std::fs::remove_dir_all(&config.data_dir).unwrap();
    let mut restarted_budget = Budget::new(1 << 28);
    let mut restarted = Engine::new(&config, &mut restarted_budget).unwrap();
    let output = run_with(
        &mut restarted,
        &mut restarted_budget,
        "SET ROLE durable_owner;
         CREATE TABLE durable_default_after_restart (value text);
         INSERT INTO durable_default_after_restart VALUES ('default-authority');
         RESET ROLE;
         SET ROLE durable_reader;
         SELECT value FROM durable_exposed;
         SELECT value FROM durable_default_after_restart;
         SELECT nextval('durable_sequence');
         RESET ROLE;
         SELECT has_schema_privilege('unprivileged', 'public', 'USAGE'),
                has_table_privilege('durable_reader', 'durable_exposed', 'SELECT'),
                has_type_privilege('unprivileged', 'durable_state', 'USAGE'),
                pg_get_userbyid(c.relowner)
           FROM pg_class c
          WHERE c.relname = 'durable_private';",
    );
    assert_eq!(
        data_rows(&output),
        [
            "object-authority",
            "default-authority",
            "1",
            "f|t|f|durable_owner",
        ],
        "{}",
        String::from_utf8_lossy(&output)
    );
    drop(restarted);
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
}

#[test]
fn foreign_keys_require_references_privilege_on_parent() {
    let (mut engine, mut budget) = test_engine();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE parent_owner;
         CREATE ROLE child_builder;
         GRANT CREATE ON SCHEMA public TO parent_owner, child_builder;
         SET ROLE parent_owner;
         CREATE TABLE referenced_parent (id int PRIMARY KEY);
         RESET ROLE;",
    );
    assert!(!String::from_utf8_lossy(&setup).contains("ERROR"));
    let denied = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE child_builder;
         CREATE TABLE denied_child (parent_id int REFERENCES referenced_parent(id));",
    );
    assert!(
        String::from_utf8_lossy(&denied).contains("permission denied for table referenced_parent"),
        "{}",
        String::from_utf8_lossy(&denied)
    );
    let allowed = run_with(
        &mut engine,
        &mut budget,
        "GRANT REFERENCES ON referenced_parent TO child_builder;
         SET ROLE child_builder;
         CREATE TABLE allowed_child (parent_id int REFERENCES referenced_parent(id));",
    );
    assert!(
        !String::from_utf8_lossy(&allowed).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&allowed)
    );
}

#[test]
fn user_defined_type_usage_defaults_to_public_and_can_be_revoked() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE type_owner;
         CREATE ROLE type_user;
         GRANT CREATE ON SCHEMA public TO type_owner, type_user;
         SET ROLE type_owner;
         CREATE TYPE deployment_state AS ENUM ('ready', 'blocked');
         RESET ROLE;",
    );
    assert!(
        !String::from_utf8_lossy(&output).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&output)
    );
    let output = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE type_user;
         CREATE TABLE default_type_access (state deployment_state);
         RESET ROLE;
         SELECT has_type_privilege('type_user', 'deployment_state', 'USAGE');",
    );
    assert!(
        !String::from_utf8_lossy(&output).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(data_rows(&output), ["t"]);
    let output = run_with(
        &mut engine,
        &mut budget,
        "REVOKE USAGE ON TYPE deployment_state FROM PUBLIC;
         SET ROLE type_user;
         CREATE TABLE denied_type_access (state deployment_state);",
    );
    let rendered = String::from_utf8_lossy(&output);
    assert!(
        rendered.contains("permission denied for type deployment_state"),
        "{rendered}"
    );
    let output = run_with(
        &mut engine,
        &mut budget,
        "SELECT has_type_privilege('type_user', 'deployment_state', 'USAGE');",
    );
    assert_eq!(data_rows(&output), ["f"]);
    let output = run_with(
        &mut engine,
        &mut budget,
        "GRANT USAGE ON TYPE deployment_state TO type_user;
         SET ROLE type_user;
         CREATE TABLE explicit_type_access (state deployment_state);",
    );
    assert!(
        !String::from_utf8_lossy(&output).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&output)
    );
}

fn message_types(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        out.push(bytes[i]);
        let len = i32::from_be_bytes(bytes[i + 1..i + 5].try_into().unwrap()) as usize;
        i += 1 + len;
    }
    out
}

fn command_tags(bytes: &[u8]) -> Vec<String> {
    let mut tags = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let message_type = bytes[index];
        let length = i32::from_be_bytes(bytes[index + 1..index + 5].try_into().unwrap()) as usize;
        if message_type == b'C' {
            let payload = &bytes[index + 5..index + 1 + length - 1];
            tags.push(core::str::from_utf8(payload).unwrap().to_owned());
        }
        index += 1 + length;
    }
    tags
}

/// Extracts text values from DataRow messages, '|'-joined per row.
fn data_rows(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let t = bytes[i];
        let len = i32::from_be_bytes(bytes[i + 1..i + 5].try_into().unwrap()) as usize;
        if t == b'D' {
            let mut row = String::new();
            let payload = &bytes[i + 5..i + 1 + len];
            let ncols = i16::from_be_bytes(payload[..2].try_into().unwrap()) as usize;
            let mut at = 2;
            for c in 0..ncols {
                if c > 0 {
                    row.push('|');
                }
                let vlen = i32::from_be_bytes(payload[at..at + 4].try_into().unwrap());
                at += 4;
                if vlen < 0 {
                    row.push_str("NULL");
                } else {
                    row.push_str(core::str::from_utf8(&payload[at..at + vlen as usize]).unwrap());
                    at += vlen as usize;
                }
            }
            out.push(row);
        }
        i += 1 + len;
    }
    out
}

#[test]
fn reset_role_uses_postgresql_reset_command_tag() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE command_tag_role; SET ROLE command_tag_role; RESET ROLE; SET ROLE NONE",
    );
    assert_eq!(
        command_tags(&output),
        ["CREATE ROLE", "SET", "RESET", "SET"]
    );
}

#[test]
fn session_authorization_is_transactional_and_resets_to_authenticated_role() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE session_owner;
         SET SESSION AUTHORIZATION session_owner;
         SELECT session_user, current_user, current_role;
         RESET ROLE;
         SELECT session_user, current_user, current_role;
         RESET SESSION AUTHORIZATION;
         SELECT session_user, current_user, current_role;
         BEGIN;
         SET LOCAL SESSION AUTHORIZATION session_owner;
         SELECT session_user, current_user;
         COMMIT;
         SELECT session_user, current_user;
         BEGIN;
         SET SESSION AUTHORIZATION session_owner;
         ROLLBACK;
         SELECT session_user, current_user;",
    );
    assert_eq!(
        data_rows(&output),
        [
            "session_owner|session_owner|session_owner",
            "session_owner|session_owner|session_owner",
            "postgres|postgres|postgres",
            "session_owner|session_owner",
            "postgres|postgres",
            "postgres|postgres",
        ],
        "{}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn reset_session_authorization_survives_a_dropped_authenticated_role() {
    let (mut engine, mut budget) = test_engine();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE authenticated_super SUPERUSER;
         CREATE ROLE authorization_alias SUPERUSER;",
    );
    assert!(!message_types(&setup).contains(&b'E'));

    let mut guc = GucState::new();
    guc.set_session_user("authenticated_super");
    let output = run_with_guc(
        &mut engine,
        &mut budget,
        "SET SESSION AUTHORIZATION authorization_alias;
         DROP ROLE authenticated_super;
         RESET SESSION AUTHORIZATION;",
        1 << 18,
        &mut guc,
    );
    assert_eq!(
        command_tags(&output),
        ["SET", "DROP ROLE", "RESET"],
        "{}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn default_privileges_apply_additively_and_replay_from_wal() {
    let config = test_config("default-acl-wal-replay");
    let mut budget = Budget::new(1 << 27);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE default_owner;
         CREATE ROLE default_reader;
         GRANT CREATE ON SCHEMA public TO default_owner;
         ALTER DEFAULT PRIVILEGES FOR ROLE default_owner
           GRANT SELECT ON TABLES TO default_reader WITH GRANT OPTION;
         ALTER DEFAULT PRIVILEGES FOR ROLE default_owner IN SCHEMA public
           GRANT INSERT ON TABLES TO default_reader;
         ALTER DEFAULT PRIVILEGES FOR ROLE default_owner
           REVOKE USAGE ON TYPES FROM PUBLIC;
         SET ROLE default_owner;
         CREATE TABLE default_table (id int);
         CREATE TYPE default_type AS ENUM ('ready');
         INSERT INTO default_table VALUES (1);
         RESET ROLE;
         SELECT defaclobjtype, defaclnamespace = 0, cardinality(defaclacl)
           FROM pg_default_acl ORDER BY defaclobjtype, defaclnamespace;
         SET ROLE default_reader;
         INSERT INTO default_table VALUES (2);
         SELECT id FROM default_table ORDER BY id;
         RESET ROLE;
         SELECT has_type_privilege('default_reader', 'default_type', 'USAGE');",
    );
    assert!(
        !String::from_utf8_lossy(&output).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(
        data_rows(&output),
        ["T|t|2", "r|t|2", "r|f|1", "1", "2", "f"],
        "{}",
        String::from_utf8_lossy(&output)
    );
    drop(engine);

    let mut restarted_budget = Budget::new(1 << 27);
    let mut restarted = Engine::new(&config, &mut restarted_budget).unwrap();
    let output = run_with(
        &mut restarted,
        &mut restarted_budget,
        "SET ROLE default_owner;
         CREATE TABLE replay_default_table (id int);
         CREATE TYPE replay_default_type AS ENUM ('ready');
         RESET ROLE;
         SET ROLE default_reader;
         INSERT INTO replay_default_table VALUES (7);
         SELECT id FROM replay_default_table;
         RESET ROLE;
         SELECT has_type_privilege('default_reader', 'replay_default_type', 'USAGE');",
    );
    assert_eq!(
        data_rows(&output),
        ["7", "f"],
        "{}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn reassign_and_drop_owned_cover_objects_grants_and_default_acls() {
    let config = test_config("reassign-drop-owned-wal");
    let mut budget = Budget::new(1 << 27);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE owned_source;
         CREATE ROLE owned_target;
         GRANT CREATE ON SCHEMA public TO owned_source;
         ALTER DEFAULT PRIVILEGES FOR ROLE owned_source
           GRANT SELECT ON TABLES TO owned_target;
         CREATE SCHEMA owned_space AUTHORIZATION owned_source;
         SET ROLE owned_source;
         CREATE TABLE public.owned_table (id int);
         CREATE TYPE public.owned_type AS ENUM ('ready');
         REVOKE USAGE ON TYPE public.owned_type FROM PUBLIC;
         RESET ROLE;",
    );
    assert!(!String::from_utf8_lossy(&setup).contains("ERROR"));
    let output = run_with(
        &mut engine,
        &mut budget,
        "REASSIGN OWNED BY owned_source TO owned_target;
         SELECT tableowner FROM pg_tables WHERE tablename = 'owned_table';
         SELECT nspname, pg_get_userbyid(nspowner)
           FROM pg_namespace WHERE nspname = 'owned_space';
         SELECT relacl::text FROM pg_class WHERE relname = 'owned_table';
         SELECT typacl::text FROM pg_type WHERE typname = 'owned_type';
         DROP OWNED BY owned_source;
         DROP ROLE owned_source;
         SELECT count(*) FROM pg_default_acl;
         DROP OWNED BY owned_target CASCADE;
         DROP ROLE owned_target;
         SELECT count(*) FROM pg_tables WHERE tablename = 'owned_table';",
    );
    assert!(
        !String::from_utf8_lossy(&output).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(
        data_rows(&output),
        [
            "owned_target",
            "owned_space|owned_target",
            "{owned_target=arwdDxtm/owned_target}",
            "{owned_target=U/owned_target}",
            "0",
            "0",
        ],
        "{}",
        String::from_utf8_lossy(&output)
    );
    drop(engine);
    let mut restarted_budget = Budget::new(1 << 27);
    let mut restarted = Engine::new(&config, &mut restarted_budget).unwrap();
    let output = run_with(
        &mut restarted,
        &mut restarted_budget,
        "SELECT count(*) FROM pg_roles
          WHERE rolname IN ('owned_source', 'owned_target');
         SELECT count(*) FROM pg_default_acl;",
    );
    assert_eq!(data_rows(&output), ["0", "0"]);
}

#[test]
fn drop_owned_restrict_refuses_cross_owner_stored_query_dependents() {
    let (mut engine, mut budget) = test_engine();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE dependency_owner;
         CREATE ROLE dependency_viewer;
         GRANT CREATE ON SCHEMA public TO dependency_owner, dependency_viewer;
         SET ROLE dependency_owner;
         CREATE TABLE dependency_table (id int);
         RESET ROLE;
         GRANT SELECT ON dependency_table TO dependency_viewer;
         SET ROLE dependency_viewer;
         CREATE VIEW dependency_view AS SELECT id FROM dependency_table;
         RESET ROLE;",
    );
    assert!(!String::from_utf8_lossy(&setup).contains("ERROR"));
    let restricted = run_with(
        &mut engine,
        &mut budget,
        "DROP OWNED BY dependency_owner RESTRICT",
    );
    assert!(
        String::from_utf8_lossy(&restricted).contains("other objects depend"),
        "{}",
        String::from_utf8_lossy(&restricted)
    );
    let cascaded = run_with(
        &mut engine,
        &mut budget,
        "DROP OWNED BY dependency_owner CASCADE;
         SELECT count(*) FROM pg_views WHERE viewname = 'dependency_view';",
    );
    assert_eq!(data_rows(&cascaded), ["0"]);
}

#[test]
fn create_insert_select_roundtrip() {
    let (mut e, mut b) = test_engine();
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE t (id int NOT NULL, name text, score float8)",
    );
    let bytes = run_with(
        &mut e,
        &mut b,
        "INSERT INTO t VALUES (1, 'alpha', 1.5), (2, 'beta', NULL), (3, NULL, 2.5)",
    );
    assert_eq!(message_types(&bytes), [b'C']);
    let bytes = run_with(&mut e, &mut b, "SELECT * FROM t ORDER BY id");
    assert_eq!(
        data_rows(&bytes),
        ["1|alpha|1.5", "2|beta|NULL", "3|NULL|2.5"]
    );
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT name, score * 2 AS double_score FROM t WHERE id <= 2 ORDER BY id DESC",
    );
    assert_eq!(data_rows(&bytes), ["beta|NULL", "alpha|3"]);
}

#[test]
fn update_and_delete() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (id int, v text)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')",
    );
    let bytes = run_with(&mut e, &mut b, "UPDATE t SET v = v || '!' WHERE id > 1");
    let types = message_types(&bytes);
    assert_eq!(types, [b'C']);
    let bytes = run_with(&mut e, &mut b, "SELECT v FROM t ORDER BY id");
    assert_eq!(data_rows(&bytes), ["a", "b!", "c!"]);
    run_with(&mut e, &mut b, "DELETE FROM t WHERE id = 2");
    let bytes = run_with(&mut e, &mut b, "SELECT id FROM t ORDER BY id");
    assert_eq!(data_rows(&bytes), ["1", "3"]);
}

#[test]
fn constraint_and_type_errors() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (id int NOT NULL, v text)");
    let bytes = run_with(&mut e, &mut b, "INSERT INTO t VALUES (NULL, 'x')");
    assert_eq!(message_types(&bytes), [b'E']);
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(text.contains("23502"), "{text}");
    let bytes = run_with(&mut e, &mut b, "SELECT * FROM missing");
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(text.contains("42P01"), "{text}");
    let bytes = run_with(&mut e, &mut b, "CREATE TABLE t (id int)");
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(text.contains("42P07"), "{text}");
    let bytes = run_with(&mut e, &mut b, "CREATE TABLE IF NOT EXISTS t (id int)");
    // NoticeResponse then CommandComplete, as in PostgreSQL.
    assert_eq!(message_types(&bytes), [b'N', b'C']);
}

#[test]
fn order_by_nulls_last_and_limit() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (v int)");
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (3),(NULL),(1),(2)");
    let bytes = run_with(&mut e, &mut b, "SELECT v FROM t ORDER BY v");
    assert_eq!(data_rows(&bytes), ["1", "2", "3", "NULL"]);
    let bytes = run_with(&mut e, &mut b, "SELECT v FROM t ORDER BY v DESC LIMIT 2");
    assert_eq!(data_rows(&bytes), ["NULL", "3"]);
}

#[test]
fn large_sort_materializes_in_shared_work_arena() {
    // A sort whose materialized rows exceed the per-connection AST arena
    // (256 KiB in run_with) must still succeed by buffering in the larger
    // shared work arena — matching PostgreSQL's in-memory sort. LIMIT keeps
    // the wire output small, so this isolates the sort buffer from the send
    // buffer: only the full materialization can overflow.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (id int, pad text)");
    let pad = "x".repeat(300);
    // 900 rows x ~320 bytes materialized ~= 288 KiB, above the 256 KiB AST
    // arena but well within the 2 MiB test work arena.
    for base in 0..30 {
        let mut sql = String::from("INSERT INTO t VALUES ");
        for i in 0..30 {
            if i > 0 {
                sql.push(',');
            }
            let id = base * 30 + i;
            sql.push_str(&format!("({id},'{pad}')"));
        }
        let bytes = run_with(&mut e, &mut b, &sql);
        assert!(
            message_types(&bytes).contains(&b'C'),
            "insert failed: {}",
            String::from_utf8_lossy(&bytes)
        );
    }
    // Materialize all 900 wide rows to sort, emit only the top 3.
    let bytes = run_with(&mut e, &mut b, "SELECT id, pad FROM t ORDER BY id LIMIT 3");
    assert!(
        !message_types(&bytes).contains(&b'E'),
        "large sort errored: {}",
        String::from_utf8_lossy(&bytes)
    );
    let rows = data_rows(&bytes);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], format!("0|{pad}"));
    assert_eq!(rows[1], format!("1|{pad}"));
    assert_eq!(rows[2], format!("2|{pad}"));
}

#[test]
fn text_coercion_on_insert() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (id int, flag bool)");
    run_with(&mut e, &mut b, "INSERT INTO t VALUES ('42', 'true')");
    let bytes = run_with(&mut e, &mut b, "SELECT id, flag FROM t");
    assert_eq!(data_rows(&bytes), ["42|t"]);
    let bytes = run_with(&mut e, &mut b, "INSERT INTO t VALUES ('zap', 'true')");
    let text = String::from_utf8_lossy(&bytes).to_string();
    // Bad text for an integer column is a data error (22P02), matching
    // PostgreSQL, not a generic type mismatch.
    assert!(text.contains("22P02"), "{text}");
}

#[test]
fn select_one_still_works() {
    let (mut e, mut b) = test_engine();
    let bytes = run_with(&mut e, &mut b, "SELECT 1");
    assert_eq!(message_types(&bytes), [b'T', b'D', b'C']);
}

/// Like run_with but with a caller-owned TxnState, so explicit
/// transactions span calls (one call ≈ one wire message).
fn run_txn(engine: &mut Engine, budget: &mut Budget, txn: &mut TxnState, sql_text: &str) -> String {
    let mut buffer = crate::mem::FixedBuf::new(budget, "send", 1 << 18).unwrap();
    let arena = Arena::new(budget, "sql", 1 << 18).unwrap();
    let mut pool = test_pool(budget);
    let mut guc = GucState::new();
    let mut responder = Responder::new(&mut buffer);
    engine
        .execute_simple(
            sql_text,
            &arena,
            txn,
            &mut pool,
            &mut test_cursors(budget),
            &mut guc,
            &mut responder,
            1,
        )
        .unwrap();
    String::from_utf8_lossy(buffer.readable()).to_string()
}

#[test]
fn explicit_rollback_discards_writes() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int, v text)");
    run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (1,'keep')");
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    assert_eq!(t.status_byte(), b'T');
    run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (2,'discard')");
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "UPDATE t SET v = 'changed' WHERE id = 1",
    );
    run_txn(&mut e, &mut b, &mut t, "DELETE FROM t WHERE id = 1");
    // Inside the txn, the changes are visible to itself.
    let out = run_txn(&mut e, &mut b, &mut t, "SELECT count(*) FROM t");
    assert!(out.contains('1'), "{out}");
    run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
    assert_eq!(t.status_byte(), b'I');
    let out = run_txn(&mut e, &mut b, &mut t, "SELECT id, v FROM t ORDER BY id");
    assert!(
        out.contains("keep") && !out.contains("discard") && !out.contains("changed"),
        "{out}"
    );
}

#[test]
fn uncommitted_create_is_invisible_to_other_sessions() {
    let (mut e, mut b) = test_engine();
    let mut a = TxnState::new(&mut b, 256).unwrap();
    let mut s = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut a, "BEGIN");
    run_txn(&mut e, &mut b, &mut a, "CREATE TABLE t (id int)");
    run_txn(&mut e, &mut b, &mut a, "INSERT INTO t VALUES (1)");
    // The creator sees its own uncommitted table.
    let own = run_txn(&mut e, &mut b, &mut a, "SELECT id FROM t");
    assert!(
        own.contains("SELECT 1"),
        "creator sees its own table: {own}"
    );
    // Another session does not.
    let other = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM t");
    assert!(
        other.contains("does not exist"),
        "other must not see it: {other}"
    );
    // Creating the same name waits for the owner, then rechecks the catalog.
    let conflict = run_txn(&mut e, &mut b, &mut s, "CREATE TABLE t (x int)");
    assert!(
        conflict.is_empty(),
        "concurrent create must park without output: {conflict}"
    );
    // After commit it becomes visible to everyone.
    run_txn(&mut e, &mut b, &mut a, "COMMIT");
    let resumed = run_txn(&mut e, &mut b, &mut s, "CREATE TABLE t (x int)");
    assert!(resumed.contains("42P07"), "{resumed}");
    let now = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM t");
    assert!(now.contains("SELECT 1"), "visible after commit: {now}");
}

#[test]
fn uncommitted_drop_stays_visible_to_other_sessions() {
    let (mut e, mut b) = test_engine();
    let mut a = TxnState::new(&mut b, 256).unwrap();
    let mut s = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut a, "CREATE TABLE t (id int)");
    run_txn(&mut e, &mut b, &mut a, "INSERT INTO t VALUES (7)");
    run_txn(&mut e, &mut b, &mut a, "BEGIN");
    let dropped = run_txn(&mut e, &mut b, &mut a, "DROP TABLE t");
    assert!(dropped.contains("DROP TABLE"), "drop succeeds: {dropped}");
    // PostgreSQL keeps the old catalog image transactionally, but DROP's
    // ACCESS EXCLUSIVE lock makes a concurrent reader wait rather than use it.
    let other = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM t");
    assert!(
        other.is_empty(),
        "concurrent SELECT waits for DROP TABLE: {other}"
    );
    run_txn(&mut e, &mut b, &mut a, "COMMIT");
    let after = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM t");
    assert!(
        after.contains("does not exist"),
        "gone after commit: {after}"
    );
}

#[test]
fn transactional_alter_table_versions_shape_and_rows() {
    let (mut engine, mut budget) = test_engine();
    let mut owner = TxnState::new(&mut budget, 256).unwrap();
    let mut observer = TxnState::new(&mut budget, 256).unwrap();
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "CREATE TABLE shaped (id int PRIMARY KEY, value text)",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "INSERT INTO shaped VALUES (1, 'old')",
    );

    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    let altered = run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "ALTER TABLE shaped ADD COLUMN generation int DEFAULT 7",
    );
    assert!(altered.contains("ALTER TABLE"), "{altered}");
    let owner_rows = data_rows(&run_with_txn_bytes(
        &mut engine,
        &mut budget,
        &mut owner,
        "SELECT id, value, generation FROM shaped",
    ));
    assert_eq!(owner_rows, ["1|old|7"]);
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut owner,
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'shaped' \
             ORDER BY ordinal_position",
        )),
        ["id", "value", "generation"]
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut observer,
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'shaped' \
             ORDER BY ordinal_position",
        )),
        ["id", "value"]
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut owner,
            "SELECT a.attname FROM pg_attribute a JOIN pg_class c \
             ON c.oid = a.attrelid WHERE c.relname = 'shaped' \
             ORDER BY a.attnum",
        )),
        ["id", "value", "generation"]
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "ALTER TABLE shaped ADD UNIQUE (generation)",
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut owner,
            "SELECT pg_get_constraintdef(c.oid, true) \
             FROM pg_constraint c JOIN pg_class r ON r.oid = c.conrelid \
             WHERE r.relname = 'shaped' AND c.conname = 'shaped_generation_key'",
        )),
        ["UNIQUE (generation)"]
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut observer,
            "SELECT count(*) FROM pg_constraint c JOIN pg_class r ON r.oid = c.conrelid \
             WHERE r.relname = 'shaped' AND c.conname = 'shaped_generation_key'",
        )),
        ["0"]
    );

    let observer_rows = run_with_txn_bytes(
        &mut engine,
        &mut budget,
        &mut observer,
        "SELECT id, value FROM shaped",
    );
    assert!(
        observer_rows.is_empty(),
        "observer waits for ALTER TABLE's ACCESS EXCLUSIVE lock"
    );

    run_txn(&mut engine, &mut budget, &mut owner, "ROLLBACK");
    let resumed_rows = data_rows(&run_with_txn_bytes(
        &mut engine,
        &mut budget,
        &mut observer,
        "SELECT id, value FROM shaped",
    ));
    assert_eq!(resumed_rows, ["1|old"]);
    let rolled_back = run_txn(
        &mut engine,
        &mut budget,
        &mut observer,
        "SELECT generation FROM shaped",
    );
    assert!(rolled_back.contains("42703"), "{rolled_back}");

    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "ALTER TABLE shaped ADD COLUMN generation int DEFAULT 9",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "UPDATE shaped SET value = 'new', generation = 10 WHERE id = 1",
    );
    run_txn(&mut engine, &mut budget, &mut owner, "COMMIT");
    let committed = data_rows(&run_with_txn_bytes(
        &mut engine,
        &mut budget,
        &mut observer,
        "SELECT id, value, generation FROM shaped",
    ));
    assert_eq!(committed, ["1|new|10"]);
}

#[test]
fn transactional_alter_table_savepoint_and_rename_visibility() {
    let (mut engine, mut budget) = test_engine();
    let mut owner = TxnState::new(&mut budget, 256).unwrap();
    let mut observer = TxnState::new(&mut budget, 256).unwrap();
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "CREATE TABLE original (id int PRIMARY KEY);
         CREATE TABLE child (parent_id int REFERENCES original(id));
         CREATE INDEX original_id_idx ON original(id);
         COMMENT ON TABLE original IS 'stable identity';
         INSERT INTO original VALUES (1);
         INSERT INTO child VALUES (1)",
    );
    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "ALTER TABLE original ADD COLUMN first int DEFAULT 2",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "SAVEPOINT before_rename",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "ALTER TABLE original RENAME COLUMN first TO second",
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut owner,
            "SELECT id, second FROM original",
        )),
        ["1|2"]
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "ROLLBACK TO SAVEPOINT before_rename",
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut owner,
            "SELECT id, first FROM original",
        )),
        ["1|2"]
    );
    run_txn(&mut engine, &mut budget, &mut owner, "COMMIT");
    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "ALTER TABLE original RENAME TO renamed",
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut owner,
            "SELECT id, first FROM renamed",
        )),
        ["1|2"]
    );
    let owner_dependencies = run_with_txn_bytes(
        &mut engine,
        &mut budget,
        &mut owner,
        "INSERT INTO child VALUES (1);
         SELECT pg_get_constraintdef(c.oid, true)
           FROM pg_constraint c JOIN pg_class r ON r.oid = c.conrelid
          WHERE r.relname = 'child' AND c.contype = 'f';
         SELECT indexname FROM pg_indexes
          WHERE tablename = 'renamed' AND indexname = 'original_id_idx';
         SELECT obj_description('renamed'::regclass, 'pg_class')",
    );
    assert_eq!(
        data_rows(&owner_dependencies),
        [
            "FOREIGN KEY (parent_id) REFERENCES renamed(id)",
            "original_id_idx",
            "stable identity"
        ],
        "{}",
        String::from_utf8_lossy(&owner_dependencies)
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut observer,
            "SELECT pg_get_constraintdef(c.oid, true)
               FROM pg_constraint c JOIN pg_class r ON r.oid = c.conrelid
              WHERE r.relname = 'child' AND c.contype = 'f'",
        )),
        ["FOREIGN KEY (parent_id) REFERENCES original(id)"]
    );
    let observer_new = run_txn(
        &mut engine,
        &mut budget,
        &mut observer,
        "SELECT * FROM renamed",
    );
    assert!(observer_new.contains("42P01"), "{observer_new}");
    let observer_old = run_with_txn_bytes(
        &mut engine,
        &mut budget,
        &mut observer,
        "SELECT id FROM original",
    );
    assert!(
        observer_old.is_empty(),
        "observer waits for ALTER TABLE RENAME's ACCESS EXCLUSIVE lock"
    );
    let committed = run_txn(&mut engine, &mut budget, &mut owner, "COMMIT");
    assert!(committed.contains("COMMIT"), "{committed}");
    let old_after_commit = run_txn(
        &mut engine,
        &mut budget,
        &mut observer,
        "SELECT id FROM original",
    );
    assert!(old_after_commit.contains("42P01"), "{old_after_commit}");
    let visible = run_with_txn_bytes(
        &mut engine,
        &mut budget,
        &mut observer,
        "SELECT id, first FROM renamed",
    );
    assert_eq!(
        data_rows(&visible),
        ["1|2"],
        "{}",
        String::from_utf8_lossy(&visible)
    );
}

#[test]
fn dropper_does_not_see_its_own_dropped_table() {
    let (mut e, mut b) = test_engine();
    let mut a = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut a, "CREATE TABLE t (id int)");
    run_txn(&mut e, &mut b, &mut a, "BEGIN");
    run_txn(&mut e, &mut b, &mut a, "DROP TABLE t");
    // Referencing the just-dropped table errors and, as in PostgreSQL,
    // aborts the transaction (so a later COMMIT rolls back).
    let own = run_txn(&mut e, &mut b, &mut a, "SELECT id FROM t");
    assert!(
        own.contains("does not exist"),
        "dropper does not see it: {own}"
    );
    assert_eq!(a.status_byte(), b'E', "the failed reference aborts the txn");
    run_txn(&mut e, &mut b, &mut a, "ROLLBACK");
}

#[test]
fn uncommitted_create_view_is_invisible_to_other_sessions() {
    let (mut e, mut b) = test_engine();
    let mut a = TxnState::new(&mut b, 256).unwrap();
    let mut s = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut a, "CREATE TABLE t (id int)");
    run_txn(&mut e, &mut b, &mut a, "INSERT INTO t VALUES (3)");
    run_txn(&mut e, &mut b, &mut a, "BEGIN");
    run_txn(&mut e, &mut b, &mut a, "CREATE VIEW v AS SELECT id FROM t");
    // The creator sees its own uncommitted view.
    let own = run_txn(&mut e, &mut b, &mut a, "SELECT id FROM v");
    assert!(
        own.contains("SELECT 1") && own.contains('3'),
        "creator sees its own view: {own}"
    );
    // Another session does not.
    let other = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM v");
    assert!(
        other.contains("does not exist"),
        "other must not see it: {other}"
    );
    // Nor can it create the same name concurrently.
    let conflict = run_txn(&mut e, &mut b, &mut s, "CREATE VIEW v AS SELECT id FROM t");
    assert!(
        conflict.is_empty(),
        "concurrent create waits without wire output: {conflict}"
    );
    // After commit it becomes visible to everyone.
    run_txn(&mut e, &mut b, &mut a, "COMMIT");
    let duplicate = run_txn(&mut e, &mut b, &mut s, "CREATE VIEW v AS SELECT id FROM t");
    assert!(
        duplicate.contains("42P07"),
        "the resumed create rechecks the committed catalog: {duplicate}"
    );
    let now = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM v");
    assert!(
        now.contains("SELECT 1") && now.contains('3'),
        "visible after commit: {now}"
    );
}

#[test]
fn rolled_back_create_view_never_appears() {
    let (mut e, mut b) = test_engine();
    let mut a = TxnState::new(&mut b, 256).unwrap();
    let mut s = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut a, "CREATE TABLE t (id int)");
    run_txn(&mut e, &mut b, &mut a, "BEGIN");
    run_txn(&mut e, &mut b, &mut a, "CREATE VIEW v AS SELECT id FROM t");
    run_txn(&mut e, &mut b, &mut a, "ROLLBACK");
    let gone = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM v");
    assert!(
        gone.contains("does not exist"),
        "rolled-back view never appears: {gone}"
    );
    // The name (and slot) is free again for anyone.
    let reuse = run_txn(&mut e, &mut b, &mut s, "CREATE VIEW v AS SELECT id FROM t");
    assert!(
        reuse.contains("CREATE VIEW"),
        "slot freed after rollback: {reuse}"
    );
}

#[test]
fn uncommitted_drop_view_stays_visible_to_other_sessions() {
    let (mut e, mut b) = test_engine();
    let mut a = TxnState::new(&mut b, 256).unwrap();
    let mut s = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut a, "CREATE TABLE t (id int)");
    run_txn(&mut e, &mut b, &mut a, "INSERT INTO t VALUES (9)");
    run_txn(&mut e, &mut b, &mut a, "CREATE VIEW v AS SELECT id FROM t");
    run_txn(&mut e, &mut b, &mut a, "BEGIN");
    let dropped = run_txn(&mut e, &mut b, &mut a, "DROP VIEW v");
    assert!(dropped.contains("DROP VIEW"), "drop succeeds: {dropped}");
    // The dropper no longer sees it; others still do until commit.
    let own = run_txn(&mut e, &mut b, &mut a, "SELECT id FROM v");
    assert!(
        own.contains("does not exist"),
        "dropper does not see it: {own}"
    );
    run_txn(&mut e, &mut b, &mut a, "ROLLBACK");
    let other = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM v");
    assert!(
        other.contains("SELECT 1") && other.contains('9'),
        "still visible after rollback: {other}"
    );
    // Now commit an actual drop and it disappears for everyone.
    run_txn(&mut e, &mut b, &mut a, "DROP VIEW v");
    let after = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM v");
    assert!(
        after.contains("does not exist"),
        "gone after committed drop: {after}"
    );
}

#[test]
fn uncommitted_create_index_is_invisible_to_other_sessions() {
    let (mut e, mut b) = test_engine();
    let mut a = TxnState::new(&mut b, 256).unwrap();
    let mut s = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut a, "CREATE TABLE t (id int)");
    run_txn(&mut e, &mut b, &mut a, "INSERT INTO t VALUES (1)");
    run_txn(&mut e, &mut b, &mut a, "BEGIN");
    run_txn(&mut e, &mut b, &mut a, "CREATE UNIQUE INDEX t_id ON t (id)");
    // The pending unique index binds its creator...
    let own = run_txn(&mut e, &mut b, &mut a, "INSERT INTO t VALUES (1)");
    assert!(
        own.contains("23505"),
        "creator is bound by its own pending index: {own}"
    );
    run_txn(&mut e, &mut b, &mut a, "ROLLBACK");
    // ...and CREATE INDEX's SHARE relation lock makes concurrent writers wait
    // even though they cannot yet see the pending index definition.
    run_txn(&mut e, &mut b, &mut a, "BEGIN");
    run_txn(&mut e, &mut b, &mut a, "CREATE UNIQUE INDEX t_id ON t (id)");
    let other = run_txn(&mut e, &mut b, &mut s, "INSERT INTO t VALUES (1)");
    assert!(
        other.is_empty(),
        "writer waits for pending index transaction: {other}"
    );
    run_txn(&mut e, &mut b, &mut a, "ROLLBACK");
    let resumed_insert = run_txn(&mut e, &mut b, &mut s, "INSERT INTO t VALUES (1)");
    assert!(
        resumed_insert.contains("INSERT 0 1"),
        "rollback removes the invisible unique index before recheck: {resumed_insert}"
    );

    // Same-name catalog operations wait and recheck too.
    run_txn(&mut e, &mut b, &mut a, "BEGIN");
    run_txn(&mut e, &mut b, &mut a, "CREATE INDEX t_id ON t (id)");
    let conflict = run_txn(&mut e, &mut b, &mut s, "CREATE INDEX t_id ON t (id)");
    assert!(conflict.is_empty(), "concurrent create waits: {conflict}");
    run_txn(&mut e, &mut b, &mut a, "ROLLBACK");
    // After rollback the resumed statement sees that the name is free.
    let reuse = run_txn(&mut e, &mut b, &mut s, "CREATE INDEX t_id ON t (id)");
    assert!(
        reuse.contains("CREATE INDEX"),
        "name freed after rollback: {reuse}"
    );
}

#[test]
fn rolled_back_create_never_appears_and_frees_the_slot() {
    let (mut e, mut b) = test_engine();
    let mut a = TxnState::new(&mut b, 256).unwrap();
    let mut s = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut a, "BEGIN");
    run_txn(&mut e, &mut b, &mut a, "CREATE TABLE r (id int)");
    run_txn(&mut e, &mut b, &mut a, "ROLLBACK");
    let gone = run_txn(&mut e, &mut b, &mut a, "SELECT id FROM r");
    assert!(
        gone.contains("does not exist"),
        "rolled-back create is gone: {gone}"
    );
    // The freed slot is reusable by a fresh create of the same name.
    let recreate = run_txn(&mut e, &mut b, &mut s, "CREATE TABLE r (x int)");
    assert!(
        recreate.contains("CREATE TABLE"),
        "slot reusable: {recreate}"
    );
}

#[test]
fn rolled_back_drop_keeps_the_table() {
    let (mut e, mut b) = test_engine();
    let mut a = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut a, "CREATE TABLE t (id int)");
    run_txn(&mut e, &mut b, &mut a, "INSERT INTO t VALUES (5)");
    run_txn(&mut e, &mut b, &mut a, "BEGIN");
    run_txn(&mut e, &mut b, &mut a, "DROP TABLE t");
    run_txn(&mut e, &mut b, &mut a, "ROLLBACK");
    let out = run_txn(&mut e, &mut b, &mut a, "SELECT id FROM t");
    assert!(
        out.contains("SELECT 1") && out.contains('5'),
        "table survives rolled-back drop: {out}"
    );
}

#[test]
fn client_min_messages_filters_by_severity() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    // Default (notice): a DROP IF EXISTS on a missing table emits a NOTICE.
    let out = run_txn(&mut e, &mut b, &mut t, "DROP TABLE IF EXISTS nope");
    assert!(
        out.contains("NOTICE") && out.contains("does not exist"),
        "{out}"
    );
    // At `warning`, the NOTICE is suppressed but a WARNING survives.
    let out = run_txn(
        &mut e,
        &mut b,
        &mut t,
        "SET client_min_messages = warning; DROP TABLE IF EXISTS nope; ROLLBACK",
    );
    assert!(
        !out.contains("does not exist"),
        "NOTICE must be filtered: {out}"
    );
    assert!(
        out.contains("WARNING") && out.contains("no transaction in progress"),
        "WARNING must survive: {out}"
    );
    // Unknown level errors like PostgreSQL (22023); a valid level shows back.
    let out = run_txn(&mut e, &mut b, &mut t, "SET client_min_messages = bogus");
    assert!(out.contains("22023"), "{out}");
    let out = run_txn(&mut e, &mut b, &mut t, "SHOW client_min_messages");
    assert!(out.contains("notice"), "{out}");
}

#[test]
fn session_gucs_honored_or_rejected_faithfully() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    // Honored (the driver/tool session-setup set): each acknowledges SET.
    for s in [
        "SET extra_float_digits = 3",
        "SET lock_timeout = 5000",
        "SET statement_timeout = 0",
        "SET idle_in_transaction_session_timeout = 0",
        "SET transaction_timeout = 0",
        "SET bytea_output = 'hex'",
        "SET intervalstyle = postgres",
        "SET synchronize_seqscans = off",
        "SET row_security = off",
        "SET check_function_bodies = false",
        "SET xmloption = content",
        "SET default_tablespace = ''",
        "SET default_table_access_method = heap",
    ] {
        assert!(run(s).contains("SET"), "should accept: {s}");
    }
    // SET then SHOW within one message (GUC state is per session/message).
    assert!(run("SET extra_float_digits = 2; SHOW extra_float_digits").contains('2'));
    assert!(run("SET lock_timeout = 5000; SHOW lock_timeout").contains("5000"));
    assert!(run("SET lock_timeout = '50ms'; SHOW lock_timeout").contains("50ms"));
    assert!(run("SET row_security = off; SHOW row_security").contains("off"));
    assert!(run("SET intervalstyle = postgres; SHOW intervalstyle").contains("postgres"));
    assert!(run("SET synchronize_seqscans = off; SHOW synchronize_seqscans").contains("off"));
    assert!(run("SET check_function_bodies = false; SHOW check_function_bodies").contains("off"));
    assert!(run("SET xmloption = content; SHOW xmloption").contains("content"));
    assert!(run("SET default_tablespace = ''; SHOW default_tablespace").contains("SHOW"));
    assert!(
        run("SET default_table_access_method = heap; SHOW default_table_access_method")
            .contains("heap")
    );
    // Rejected loudly — never accepted-and-ignored.
    assert!(
        run("SET extra_float_digits = 9").contains("22023"),
        "out of range"
    );
    // statement_timeout is now accepted (enforced at scan boundaries); a
    // malformed value is still rejected loudly.
    assert!(run("SET statement_timeout = 5000; SHOW statement_timeout").contains("5000"));
    assert!(
        run("SET lock_timeout = 'bogus'").contains("22023"),
        "bad lock timeout"
    );
    assert!(
        run("SET statement_timeout = 'bogus'").contains("22023"),
        "bad timeout"
    );
    // bytea_output escape is honored (verified against PostgreSQL 18.4);
    // an unknown format is rejected loudly. The GUC store is per-batch in
    // this harness, so SET and SELECT share one statement string.
    let escaped = run("SET bytea_output = 'escape'; SELECT '\\x5c00'::bytea");
    assert!(escaped.contains("\\\\000"), "escape rendering: {escaped}");
    assert!(
        run("SET bytea_output = 'bogus'").contains("22023"),
        "unknown format"
    );
    assert!(
        run("SET intervalstyle = sql_standard").contains("0A000"),
        "unsupported style"
    );
    assert!(
        run("SET synchronize_seqscans = on").contains("0A000"),
        "unsupported scan mode"
    );
    assert!(
        run("SET xmloption = document").contains("0A000"),
        "unsupported XML mode"
    );
    assert!(
        run("SET default_tablespace = fast").contains("0A000"),
        "unsupported tablespace"
    );
    assert!(
        run("SET default_table_access_method = columnar").contains("0A000"),
        "unsupported table access method"
    );
}

#[test]
fn cast_with_type_modifier() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    // numeric cast rounds to scale
    assert!(
        run("SELECT 12.345::numeric(5,1)").contains("12.3"),
        "numeric scale"
    );
    // varchar cast TRUNCATES (not error), unlike column assignment — matches PG
    assert!(
        run("SELECT 'hello'::varchar(3)").contains("hel"),
        "varchar truncate"
    );
    // SQL-standard CAST(x AS type(mod)) form
    assert!(
        run("SELECT CAST(1.5 AS numeric(10,2))").contains("1.50"),
        "CAST form"
    );
    // numeric precision overflow errors (22003)
    assert!(
        run("SELECT 123.45::numeric(3,1)").contains("22003"),
        "overflow"
    );
    // a cast without a modifier still parses
    assert!(run("SELECT 5::int8").contains('5'), "plain cast");
}

#[test]
fn set_show_transaction_and_show_all() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    // Transaction-control SET forms that JDBC/tools send.
    assert!(run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE").contains("SET"));
    assert!(
        run("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .contains("SET")
    );
    assert!(run("SET TRANSACTION READ ONLY").contains("SET"));
    assert!(run("SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE").contains("SET"));
    assert!(run("BEGIN ISOLATION LEVEL REPEATABLE READ").contains("BEGIN"));
    assert!(run("ROLLBACK").contains("ROLLBACK"));
    assert!(run("START TRANSACTION READ ONLY").contains("BEGIN"));
    assert!(run("ROLLBACK").contains("ROLLBACK"));
    assert!(
        run("BEGIN ISOLATION LEVEL READ COMMITTED, READ WRITE, NOT DEFERRABLE").contains("BEGIN")
    );
    assert!(run("ROLLBACK").contains("ROLLBACK"));
    // SQL-standard multi-word SHOW forms.
    assert!(run("SHOW TRANSACTION ISOLATION LEVEL").contains("read committed"));
    assert!(run("SHOW ALL").contains("client_encoding"));
}

#[test]
fn smallint_varchar_char_type_fidelity() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    run("CREATE TABLE ty (s smallint, v varchar(3), c char(5))");
    // smallint enforces ±32767 — the previously-silent out-of-range case.
    assert!(run("INSERT INTO ty(s) VALUES (40000)").contains("smallint out of range"));
    assert!(run("INSERT INTO ty(s) VALUES (32767)").contains("INSERT"));
    assert!(
        run("SELECT s FROM ty WHERE s = 32767").contains("32767"),
        "round-trips"
    );
    // varchar length errors; char(n) padding is *not* part of the value —
    // PostgreSQL strips it through operators, so concatenation sees "hi".
    assert!(run("INSERT INTO ty(v) VALUES ('toolong')").contains("22001"));
    assert!(
        run("INSERT INTO ty(c) VALUES ('hi'); SELECT '[' || c || ']' FROM ty WHERE c IS NOT NULL")
            .contains("[hi]"),
        "char(5) padding strips through concatenation"
    );
    assert!(
        run("SELECT length(c) FROM ty WHERE c IS NOT NULL").contains('2'),
        "length ignores char(n) padding"
    );
    assert!(
        run("SELECT count(*) FROM ty WHERE c = 'hi'").contains('1'),
        "char(n) compares equal to its stripped text"
    );
}

#[test]
fn join_using_clause() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    run("CREATE TABLE a (id int, x text)");
    run("CREATE TABLE bb (id int, y text)");
    run("INSERT INTO a VALUES (1,'a1'),(2,'a2')");
    run("INSERT INTO bb VALUES (1,'b1'),(3,'b3')");
    // JOIN ... USING (id) is desugared to ON a.id = bb.id.
    let out = run("SELECT a.x, bb.y FROM a JOIN bb USING (id)");
    assert!(out.contains("a1") && out.contains("b1"), "match: {out}");
    assert!(
        !out.contains("a2") && !out.contains("b3"),
        "non-match dropped: {out}"
    );
}

#[test]
fn serial_auto_increment() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    run("CREATE TABLE u (id serial PRIMARY KEY, name text)");
    assert!(
        run("SELECT sequencename FROM pg_sequences WHERE sequencename='u_id_seq'")
            .contains("u_id_seq")
    );
    // An omitted serial column is auto-assigned; RETURNING sees it.
    assert!(run("INSERT INTO u(name) VALUES ('a') RETURNING id").contains('1'));
    // A multi-row insert assigns increasing ids.
    let out = run("INSERT INTO u(name) VALUES ('b'),('c') RETURNING id");
    assert!(out.contains('2') && out.contains('3'), "sequential: {out}");
    // An explicit value does NOT advance the sequence (PostgreSQL: the
    // sequence is independent of the column's stored values).
    run("INSERT INTO u VALUES (100, 'd')");
    assert!(run("INSERT INTO u(name) VALUES ('e') RETURNING id").contains('4'));
    assert!(run("SELECT count(*) FROM u").contains('5'));
    // TRUNCATE keeps the sequence; RESTART IDENTITY resets it.
    assert!(run("TRUNCATE u").contains("TRUNCATE TABLE"));
    assert!(run("INSERT INTO u(name) VALUES ('f') RETURNING id").contains('5'));
    assert!(run("TRUNCATE u RESTART IDENTITY").contains("TRUNCATE TABLE"));
    assert!(run("INSERT INTO u(name) VALUES ('g') RETURNING id").contains('1'));
}

#[test]
fn on_conflict_do_nothing() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    run("CREATE TABLE kv (k int PRIMARY KEY, v text)");
    run("INSERT INTO kv VALUES (1,'a'),(2,'b')");
    // The conflicting row is skipped, the new one inserted; the count
    // excludes skips (INSERT 0 1), matching PostgreSQL.
    assert!(
        run("INSERT INTO kv VALUES (1,'x'),(3,'c') ON CONFLICT DO NOTHING").contains("INSERT 0 1")
    );
    let out = run("SELECT k, v FROM kv ORDER BY k");
    // k=1 keeps its original 'a' (the conflicting 'x' was skipped); k=3 added.
    assert!(out.contains("SELECT 3"), "three rows: {out}");
    assert!(
        out.contains('a') && out.contains('c') && !out.contains('x'),
        "kept original: {out}"
    );
    // A fully-conflicting insert stores nothing.
    assert!(run("INSERT INTO kv VALUES (2,'y') ON CONFLICT (k) DO NOTHING").contains("INSERT 0 0"));
    // DO UPDATE is a real upsert; assignments can reference the existing
    // row and excluded.<col> (the proposed row).
    run("INSERT INTO kv VALUES (1,'z') ON CONFLICT (k) DO UPDATE SET v = excluded.v");
    assert!(
        run("SELECT v FROM kv WHERE k = 1").contains('z'),
        "upserted"
    );
    // DO UPDATE ... WHERE can veto the update.
    run("INSERT INTO kv VALUES (1,'q') ON CONFLICT (k) DO UPDATE SET v = 'q' WHERE FALSE");
    assert!(
        !run("SELECT v FROM kv WHERE k = 1").contains('q'),
        "WHERE vetoed"
    );
}

#[test]
fn on_conflict_arbiter_and_returning() {
    // Arbiter resolution (matching PostgreSQL 18.4): the conflict is caught only
    // on the inferred/named unique, RETURNING projects the post-update row, and
    // the analysis errors fire regardless of the data.
    let (mut e, mut b) = test_engine();
    macro_rules! run {
        ($sql:expr) => {
            run_with(&mut e, &mut b, $sql)
        };
    }
    let err = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    run!("CREATE TABLE oc (a int UNIQUE, b int UNIQUE, note text)");
    run!("INSERT INTO oc VALUES (1,10,'x'),(2,20,'y')");

    // DO UPDATE ... RETURNING returns the updated row's post-update values.
    let out = run!(
        "INSERT INTO oc VALUES (1,10,'z') ON CONFLICT (a) DO UPDATE SET note='upd' RETURNING a,b,note"
    );
    assert_eq!(data_rows(&out), ["1|10|upd"]);

    // ON CONSTRAINT names the arbiter directly (auto-named single-col unique).
    let out = run!(
        "INSERT INTO oc VALUES (99,20,'z') ON CONFLICT ON CONSTRAINT oc_b_key DO UPDATE SET note='byname' RETURNING a,b,note"
    );
    assert_eq!(data_rows(&out), ["2|20|byname"]);

    // A conflict on a DIFFERENT unique than the arbiter is not caught — it
    // falls through to a normal duplicate-key error.
    assert!(
        err(&run!(
            "INSERT INTO oc VALUES (1,999,'d') ON CONFLICT (b) DO UPDATE SET note='no'"
        ))
        .contains("23505"),
        "non-arbiter conflict is 23505"
    );

    // Analysis errors, independent of whether a row conflicts:
    assert!(
        err(&run!(
            "INSERT INTO oc VALUES (1,10,'q') ON CONFLICT DO UPDATE SET note='q'"
        ))
        .contains("42601"),
        "DO UPDATE needs an arbiter"
    );
    assert!(
        err(&run!(
            "INSERT INTO oc VALUES (1,10,'q') ON CONFLICT (note) DO NOTHING"
        ))
        .contains("42P10"),
        "target must be unique"
    );
    assert!(
        err(&run!(
            "INSERT INTO oc VALUES (1,10,'q') ON CONFLICT (nope) DO NOTHING"
        ))
        .contains("42703"),
        "target column must exist"
    );
    assert!(
        err(&run!(
            "INSERT INTO oc VALUES (1,10,'q') ON CONFLICT ON CONSTRAINT nope DO NOTHING"
        ))
        .contains("42704"),
        "named constraint must exist"
    );

    // Composite-key arbiter matches order-independently; ON CONSTRAINT by pkey.
    run!("CREATE TABLE cc (x int, y int, v text, PRIMARY KEY (x,y))");
    run!("INSERT INTO cc VALUES (1,2,'a')");
    let out = run!(
        "INSERT INTO cc VALUES (1,2,'b') ON CONFLICT (y,x) DO UPDATE SET v=excluded.v RETURNING x,y,v"
    );
    assert_eq!(data_rows(&out), ["1|2|b"]);
    let out = run!(
        "INSERT INTO cc VALUES (1,2,'multi'),(3,4,'fresh') ON CONFLICT ON CONSTRAINT cc_pkey DO UPDATE SET v=excluded.v RETURNING x,y,v"
    );
    assert_eq!(data_rows(&out), ["1|2|multi", "3|4|fresh"]);
}

#[test]
fn multi_column_unique_and_primary_key() {
    // SQLSTATEs verified against PostgreSQL 18.4: duplicate multi-column key
    // is 23505; a NULL member makes the tuple distinct (no conflict).
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    run("CREATE TABLE t (a int, b int, c text, PRIMARY KEY (a, b))");
    assert!(run("INSERT INTO t VALUES (1, 2, 'x')").contains("INSERT 0 1"));
    // Same (a,b) tuple conflicts; a different tuple is fine.
    assert!(
        run("INSERT INTO t VALUES (1, 2, 'y')").contains("23505"),
        "dup PK"
    );
    assert!(
        run("INSERT INTO t VALUES (1, 3, 'y')").contains("INSERT 0 1"),
        "distinct"
    );
    // A PRIMARY KEY column is NOT NULL.
    assert!(
        run("INSERT INTO t VALUES (NULL, 4, 'z')").contains("23502"),
        "PK not null"
    );
    // Multi-column UNIQUE allows NULLs (distinct), rejects full duplicates.
    run("CREATE TABLE u (a int, b int, UNIQUE (a, b))");
    assert!(run("INSERT INTO u VALUES (1, NULL)").contains("INSERT 0 1"));
    assert!(
        run("INSERT INTO u VALUES (1, NULL)").contains("INSERT 0 1"),
        "NULL distinct"
    );
    assert!(run("INSERT INTO u VALUES (5, 6)").contains("INSERT 0 1"));
    assert!(
        run("INSERT INTO u VALUES (5, 6)").contains("23505"),
        "dup UNIQUE"
    );
}

#[test]
fn check_constraints_enforced() {
    // 23514 on violation; NULL passes (three-valued logic) — matches PG 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    run("CREATE TABLE c (x int CHECK (x > 0), y int, CHECK (y < 100))");
    assert!(run("INSERT INTO c VALUES (5, 10)").contains("INSERT 0 1"));
    assert!(
        run("INSERT INTO c VALUES (-1, 10)").contains("23514"),
        "x>0 violated"
    );
    assert!(
        run("INSERT INTO c VALUES (5, 200)").contains("23514"),
        "y<100 violated"
    );
    // A NULL makes the predicate NULL, which passes.
    assert!(
        run("INSERT INTO c VALUES (NULL, 10)").contains("INSERT 0 1"),
        "null passes"
    );
    // UPDATE is checked too.
    assert!(
        run("UPDATE c SET x = -5 WHERE x = 5").contains("23514"),
        "update checked"
    );
    // A CHECK referencing an unknown column is rejected at creation (42703).
    assert!(run("CREATE TABLE bad (x int CHECK (nope > 0))").contains("42703"));
}

#[test]
fn foreign_key_referential_integrity() {
    // All SQLSTATEs verified against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    run("CREATE TABLE p (id int PRIMARY KEY, name text)");
    run("CREATE TABLE ch (pid int REFERENCES p(id), note text)");
    // A referencing row with no parent is rejected (23503).
    assert!(
        run("INSERT INTO ch VALUES (5, 'orphan')").contains("23503"),
        "missing parent"
    );
    // A NULL foreign key passes (MATCH SIMPLE).
    assert!(
        run("INSERT INTO ch VALUES (NULL, 'ok')").contains("INSERT 0 1"),
        "null fk"
    );
    // With the parent present, the child inserts.
    run("INSERT INTO p VALUES (1, 'a')");
    assert!(run("INSERT INTO ch VALUES (1, 'child')").contains("INSERT 0 1"));
    // Deleting a referenced parent row is blocked (23503).
    assert!(
        run("DELETE FROM p WHERE id = 1").contains("23503"),
        "delete blocked"
    );
    // Changing the referenced key of a referenced parent is blocked.
    assert!(
        run("UPDATE p SET id = 2 WHERE id = 1").contains("23503"),
        "key change blocked"
    );
    // An unreferenced parent row can be deleted.
    run("INSERT INTO p VALUES (9, 'free')");
    assert!(
        run("DELETE FROM p WHERE id = 9").contains("DELETE 1"),
        "free delete"
    );
    // A foreign key must reference a unique/PK column set (42830).
    run("CREATE TABLE nu (a int)");
    assert!(
        run("CREATE TABLE nc (a int REFERENCES nu(a))").contains("42830"),
        "non-unique"
    );
    // Referencing a missing table is 42P01.
    assert!(
        run("CREATE TABLE nt (a int REFERENCES nope(a))").contains("42P01"),
        "missing tbl"
    );
    // Referential actions rewrite the referencing rows (verified against
    // PostgreSQL 18.4): CASCADE removes them with the parent.
    assert!(
        run("CREATE TABLE cc (pid int REFERENCES p(id) ON DELETE CASCADE)")
            .contains("CREATE TABLE"),
        "cascade accepted"
    );
    run("INSERT INTO p VALUES (5, 'x')");
    run("INSERT INTO cc VALUES (5)");
    assert!(
        run("DELETE FROM p WHERE id = 5").contains("DELETE 1"),
        "cascade delete"
    );
    let left = run("SELECT pid FROM cc");
    assert!(left.contains("SELECT 0"), "child cascaded: {left}");
}

#[test]
fn right_and_full_outer_joins() {
    // Expected rows verified against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE a (id int, x text)");
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE bt (id int, y text)");
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "INSERT INTO a VALUES (1,'a1'),(2,'a2'),(3,'a3')",
    );
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "INSERT INTO bt VALUES (2,'b2'),(3,'b3'),(4,'b4')",
    );
    // RIGHT JOIN preserves the right side; the unmatched b4 nulls a.x.
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT a.x FROM a RIGHT JOIN bt ON a.id=bt.id ORDER BY bt.id",
    ));
    assert_eq!(
        rows,
        ["a2", "a3", "NULL"],
        "right unmatched nulls left: {rows:?}"
    );
    // FULL JOIN preserves both: unmatched a1 (left) and unmatched b4 (right).
    let full = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT coalesce(a.x,'-'), coalesce(bt.y,'-') FROM a FULL JOIN bt ON a.id=bt.id ORDER BY a.id NULLS LAST, bt.id",
    ));
    assert_eq!(full, ["a1|-", "a2|b2", "a3|b3", "-|b4"], "full: {full:?}");
}

#[test]
fn time_type() {
    // Output verified against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int, tm time)");
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "INSERT INTO t VALUES (1,'12:34:56'),(2,'09:00:00'),(3,'23:59:59.5')",
    );
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT id, tm FROM t ORDER BY tm",
    ));
    assert_eq!(
        rows,
        ["2|09:00:00", "1|12:34:56", "3|23:59:59.5"],
        "ordered: {rows:?}"
    );
    // Casts: text -> time, and the time-of-day of a timestamp.
    assert!(run_txn(&mut e, &mut b, &mut t, "SELECT '08:30'::time").contains("08:30:00"));
    assert!(
        run_txn(
            &mut e,
            &mut b,
            &mut t,
            "SELECT '2024-01-15 14:30:00'::timestamp::time"
        )
        .contains("14:30:00")
    );
}

#[test]
fn array_type() {
    // Output/operators verified against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (a int[])");
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "INSERT INTO t VALUES ('{1,2,3}'),(ARRAY[4,5])",
    );
    // Literal output and storage roundtrip with ORDER BY (element-wise).
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT a FROM t ORDER BY a",
    ));
    assert_eq!(rows, ["{1,2,3}", "{4,5}"], "array storage/order: {rows:?}");
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    assert!(run("SELECT ARRAY[1,2,3]").contains("{1,2,3}"));
    assert!(run("SELECT '{4,5,6}'::int[]").contains("{4,5,6}"));
    assert!(run("SELECT ARRAY['a','b']").contains("{a,b}"));
    assert!(run("SELECT '{x,y z}'::text[]").contains("{x,\"y z\"}"));
    // 1-based subscript, length/cardinality, and = ANY.
    assert!(run("SELECT (ARRAY[10,20,30])[2]").contains("20"));
    assert!(run("SELECT array_length(ARRAY[1,2,3],1)").contains('3'));
    assert!(run("SELECT cardinality(ARRAY[1,2,3])").contains('3'));
    assert!(run("SELECT 20 = ANY(ARRAY[10,20,30])").contains('t'));
    assert!(run("SELECT 99 = ANY(ARRAY[10,20,30])").contains('f'));
    // Array slicing a[lo:hi], with optional bounds and clamping.
    assert!(run("SELECT (ARRAY[1,2,3,4,5])[2:4]").contains("{2,3,4}"));
    assert!(run("SELECT (ARRAY[1,2,3,4,5])[:3]").contains("{1,2,3}"));
    assert!(run("SELECT (ARRAY[1,2,3,4,5])[3:]").contains("{3,4,5}"));
    assert!(run("SELECT (ARRAY[1,2,3])[2:10]").contains("{2,3}"));
    assert!(run("SELECT (ARRAY[1,2,3])[5:10]").contains("{}"));
    assert!(run("SELECT (ARRAY[1,2,3])[NULL:2] IS NULL").contains('t'));
    // A slice keeps the array type (not the element type).
    assert!(run("SELECT pg_typeof((ARRAY[1,2,3])[1:2])").contains("integer[]"));
}

#[test]
fn json_and_jsonb_types() {
    // Output/normalization/operators verified against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    // json is verbatim; jsonb normalizes (sorted keys, last-wins dedup,
    // canonical spacing and numbers).
    assert!(
        run("SELECT '{\"b\": 1,  \"a\":2, \"b\":3}'::json")
            .contains("{\"b\": 1,  \"a\":2, \"b\":3}")
    );
    assert!(run("SELECT '{\"b\": 1,  \"a\":2, \"b\":3}'::jsonb").contains("{\"a\": 2, \"b\": 3}"));
    assert!(run("SELECT '[1, 2,   3]'::jsonb").contains("[1, 2, 3]"));
    assert!(run("SELECT '1e2'::jsonb").contains("100"));
    // -> keeps json/jsonb, ->> returns text; array index is 0-based.
    assert!(run("SELECT ('{\"a\":{\"x\":5},\"b\":[10,20]}'::jsonb)->'a'").contains("{\"x\": 5}"));
    assert!(run("SELECT ('{\"a\":5}'::jsonb)->>'a'").contains('5'));
    assert!(run("SELECT ('[10,20,30]'::jsonb)->1").contains("20"));
    // Invalid json is rejected loudly.
    assert!(run("SELECT '{bad}'::jsonb").contains("22P02"));
    // Date-time types render in ISO 8601 inside JSON (a `T` separator, and a
    // `+00:00` offset for timestamptz), not the space-separated `::text` form.
    assert!(
        run("SELECT to_json('2020-01-01 12:30:45'::timestamp)").contains("\"2020-01-01T12:30:45\"")
    );
    assert!(
        run("SELECT to_json('2020-01-01 12:30:45.1'::timestamp)")
            .contains("\"2020-01-01T12:30:45.1\"")
    );
    assert!(
        run("SELECT to_json('2020-01-01 12:30:45+00'::timestamptz)")
            .contains("\"2020-01-01T12:30:45+00:00\"")
    );
    assert!(
        run("SELECT to_jsonb('2020-06-15 08:00:00+00'::timestamptz)")
            .contains("\"2020-06-15T08:00:00+00:00\"")
    );
    // date / time keep their ordinary text form.
    assert!(run("SELECT to_json('2020-06-15'::date)").contains("\"2020-06-15\""));
    // jsonb deep containment @> / <@ (object subset, array membership incl. the
    // bare-primitive exception, numeric-value equality, and type mismatch).
    assert!(run("SELECT '{\"a\":1,\"b\":2}'::jsonb @> '{\"a\":1}'::jsonb").contains('t'));
    assert!(run("SELECT '{\"a\":1}'::jsonb @> '{\"a\":1,\"b\":2}'::jsonb").contains('f'));
    assert!(run("SELECT '[1,2,3]'::jsonb @> '2'::jsonb").contains('t'));
    assert!(run("SELECT '[{\"a\":1}]'::jsonb @> '{\"a\":1}'::jsonb").contains('f'));
    assert!(run("SELECT '1.0'::jsonb @> '1'::jsonb").contains('t'));
    assert!(run("SELECT '{\"a\":1}'::jsonb <@ '{\"a\":1,\"b\":2}'::jsonb").contains('t'));
    // plain json has no containment operator.
    assert!(run("SELECT '{}'::json @> '{}'::json").contains("42883"));
}

#[test]
fn interval_type() {
    // Output/arithmetic verified against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    // Output formatting for the various field combinations.
    assert!(run("SELECT '1 day'::interval").contains("1 day"));
    assert!(run("SELECT '2 hours 30 minutes'::interval").contains("02:30:00"));
    assert!(run("SELECT '1 year 2 months'::interval").contains("1 year 2 mons"));
    assert!(run("SELECT '90 minutes'::interval").contains("01:30:00"));
    assert!(run("SELECT '-5 days'::interval").contains("-5 days"));
    // Arithmetic: date/timestamp + interval, month clamping.
    assert!(run("SELECT date '2024-01-15' + '1 day'::interval").contains("2024-01-16 00:00:00"));
    assert!(
        run("SELECT timestamp '2024-01-15 10:00' + '2 hours'::interval")
            .contains("2024-01-15 12:00:00")
    );
    assert!(
        run("SELECT timestamp '2024-03-31 10:00' + '1 month'::interval")
            .contains("2024-04-30 10:00:00")
    );
    // interval - interval.
    assert!(
        run("SELECT '1 day 2 hours'::interval - '3 hours'::interval").contains("1 day -01:00:00")
    );
}

#[test]
fn correlated_subquery_over_aliased_table_and_values_setop() {
    // A correlated scalar subquery whose outer table is aliased must
    // describe/execute (regression: describe resolved the qualifier against
    // the table name, not the alias). And VALUES as a UNION branch.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    run("CREATE TABLE p (id int)");
    run("CREATE TABLE ch (pid int)");
    run("INSERT INTO p VALUES (1),(2)");
    run("INSERT INTO ch VALUES (1),(1),(2)");
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT x.id, (SELECT count(*) FROM ch WHERE ch.pid = x.id) FROM p x ORDER BY x.id",
    ));
    assert_eq!(
        rows,
        ["1|2", "2|1"],
        "aliased correlated subquery: {rows:?}"
    );
    // VALUES in a UNION branch.
    let vals = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT 1 UNION ALL VALUES (2),(3) ORDER BY 1",
    ));
    assert_eq!(vals, ["1", "2", "3"], "values in union: {vals:?}");
}

#[test]
fn set_operations_in_subqueries() {
    // Set-operation queries in IN / scalar / derived-table / EXISTS position.
    // Semantics verified against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    // IN over a UNION ALL (with a VALUES branch).
    assert!(run("SELECT 42 WHERE 3 IN (SELECT 2 UNION ALL VALUES (3))").contains("42"));
    assert!(!run("SELECT 42 WHERE 9 IN (SELECT 2 UNION ALL VALUES (3))").contains("42"));
    // Scalar subquery collapsing a UNION to one row.
    assert!(run("SELECT (SELECT 5 UNION SELECT 5)").contains('5'));
    // Derived table over a UNION ALL.
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut e,
            &mut b,
            &mut t,
            "SELECT sum(x) FROM (SELECT 1 x UNION ALL SELECT 2 UNION ALL SELECT 3) t"
        )),
        ["6"]
    );
    // EXISTS and INTERSECT / EXCEPT.
    assert!(
        run_txn(
            &mut e,
            &mut b,
            &mut t,
            "SELECT 9 WHERE EXISTS (SELECT 1 UNION ALL SELECT 2)"
        )
        .contains('9')
    );
    assert!(
        run_txn(
            &mut e,
            &mut b,
            &mut t,
            "SELECT 7 WHERE 2 IN (SELECT 2 INTERSECT SELECT 2)"
        )
        .contains('7')
    );
    assert!(
        !run_txn(
            &mut e,
            &mut b,
            &mut t,
            "SELECT 7 WHERE 2 IN (SELECT 2 EXCEPT SELECT 2)"
        )
        .contains('7')
    );
}

#[test]
fn array_from_subquery_and_array_to_string() {
    // ARRAY(subquery) constructor and array_to_string, vs PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (x int)");
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "INSERT INTO t VALUES (10),(20),(30)",
    );
    // Elements follow the table's physical (insertion) scan order, matching
    // PostgreSQL. (ORDER BY inside a subquery is not yet honored — tracked
    // separately — so it is deliberately not exercised here.)
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut e,
            &mut b,
            &mut t,
            "SELECT array(SELECT x FROM t)"
        )),
        ["{10,20,30}"]
    );
    // Empty subquery yields an empty array, not NULL.
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut e,
            &mut b,
            &mut t,
            "SELECT array(SELECT x FROM t WHERE x > 100)"
        )),
        ["{}"]
    );
    // array_to_string joins, with and without a null replacement.
    assert!(
        run_txn(
            &mut e,
            &mut b,
            &mut t,
            "SELECT array_to_string(ARRAY[1,NULL,3], ',', '*')"
        )
        .contains("1,*,3")
    );
    assert!(
        run_txn(
            &mut e,
            &mut b,
            &mut t,
            "SELECT array_to_string(ARRAY[1,NULL,3], ',')"
        )
        .contains("1,3")
    );
}

#[test]
fn generate_series_table_function() {
    // generate_series in FROM, vs PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut e,
            &mut b,
            &mut t,
            "SELECT s FROM generate_series(0,3) s ORDER BY s"
        )),
        ["0", "1", "2", "3"]
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut e,
            &mut b,
            &mut t,
            "SELECT s FROM generate_series(1,10,2) s ORDER BY s"
        )),
        ["1", "3", "5", "7", "9"]
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut e,
            &mut b,
            &mut t,
            "SELECT s FROM generate_series(5,1,-2) s ORDER BY s DESC"
        )),
        ["5", "3", "1"]
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut e,
            &mut b,
            &mut t,
            "SELECT count(*) FROM generate_series(1,100) g"
        )),
        ["100"]
    );
}

#[test]
fn catalog_indexes_and_constraints_for_psql_d() {
    // The pg_class/pg_index/pg_constraint rows and pg_get_indexdef /
    // pg_get_constraintdef / oid::regclass that psql `\d <table>` reads,
    // verified against PostgreSQL 18.4's rendering.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    run("CREATE TABLE parent (a int, b int, PRIMARY KEY (a,b))");
    run(
        "CREATE TABLE child (id int PRIMARY KEY, pa int, pb int, email text UNIQUE, \
         FOREIGN KEY (pa,pb) REFERENCES parent(a,b))",
    );
    // Index relations exist with PostgreSQL-style names.
    let index = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT relname FROM pg_class WHERE relkind = 'i' ORDER BY relname",
    ));
    assert_eq!(
        index,
        ["child_email_key", "child_pkey", "parent_pkey"],
        "index rels: {index:?}"
    );
    // pg_get_indexdef reconstructs the btree column list.
    let pk = run_txn(
        &mut e,
        &mut b,
        &mut t,
        "SELECT pg_get_indexdef(indexrelid, 0, true) FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'parent_pkey'",
    );
    assert!(pk.contains("btree (a, b)"), "indexdef: {pk}");
    // The foreign key: constraint def + parent name via oid::regclass.
    let fk = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT confrelid::regclass, pg_get_constraintdef(oid, true) \
         FROM pg_constraint WHERE contype = 'f'",
    ));
    assert_eq!(
        fk,
        ["parent|FOREIGN KEY (pa, pb) REFERENCES parent(a, b)"],
        "fk: {fk:?}"
    );
    // A UNIQUE constraint is backed by an index (conindid links them).
    let uq = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT conname FROM pg_constraint WHERE contype = 'u' ORDER BY conname",
    ));
    assert_eq!(uq, ["child_email_key"], "unique constraints: {uq:?}");
}

#[test]
fn catalog_definitions_do_not_silently_truncate() {
    let (mut engine, mut budget) = test_engine();
    let mut transaction = TxnState::new(&mut budget, 256).unwrap();
    let columns = [
        "deliberately_long_column_name_number_01",
        "deliberately_long_column_name_number_02",
        "deliberately_long_column_name_number_03",
        "deliberately_long_column_name_number_04",
        "deliberately_long_column_name_number_05",
        "deliberately_long_column_name_number_06",
        "deliberately_long_column_name_number_07",
        "deliberately_long_column_name_number_08",
    ];
    let definitions = columns
        .iter()
        .map(|name| format!("{name} integer"))
        .collect::<Vec<_>>()
        .join(", ");
    let names = columns.join(", ");
    let parent_result = run_txn(
        &mut engine,
        &mut budget,
        &mut transaction,
        &format!("CREATE TABLE long_parent ({definitions}, PRIMARY KEY ({names}))"),
    );
    assert!(!parent_result.contains("ERROR"), "{parent_result}");
    let child_result = run_txn(
        &mut engine,
        &mut budget,
        &mut transaction,
        &format!(
            "CREATE TABLE long_child ({definitions}, \
             CONSTRAINT long_child_parent_fkey \
             FOREIGN KEY ({names}) REFERENCES long_parent ({names}))"
        ),
    );
    assert!(!child_result.contains("ERROR"), "{child_result}");

    let index_definition = data_rows(&run_with_txn_bytes(
        &mut engine,
        &mut budget,
        &mut transaction,
        "SELECT pg_get_indexdef(conindid, 0, true) FROM pg_constraint \
         WHERE contype='p' AND conrelid='long_parent'::regclass",
    ));
    assert!(
        index_definition[0].ends_with("deliberately_long_column_name_number_08)"),
        "index definition was truncated: {}",
        index_definition[0]
    );
    let foreign_key_definition = data_rows(&run_with_txn_bytes(
        &mut engine,
        &mut budget,
        &mut transaction,
        "SELECT pg_get_constraintdef(oid, true) FROM pg_constraint \
         WHERE contype='f' AND conrelid='long_child'::regclass",
    ));
    assert!(
        foreign_key_definition[0].ends_with("deliberately_long_column_name_number_08)"),
        "foreign key definition was truncated: {}",
        foreign_key_definition[0]
    );
}

#[test]
fn bitwise_operators_and_string_syntax() {
    // Bitwise operators and SQL trim/substring syntax used by JDBC's
    // DatabaseMetaData queries. Semantics verified against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    assert!(run("SELECT 6 & 3").contains('2'));
    assert!(run("SELECT 6 | 1").contains('7'));
    assert!(run("SELECT 6 # 3").contains('5'));
    assert!(run("SELECT 1 << 4").contains("16"));
    assert!(run("SELECT 32 >> 2").contains('8'));
    // `substring(str FROM start FOR len)` and `trim([dir] chars FROM str)`.
    assert!(run("SELECT substring('abcdef' FROM 2 FOR 3)").contains("bcd"));
    assert!(run("SELECT trim(both 'x' FROM 'xxhixx')").contains("hi"));
    assert!(run("SELECT trim(leading '0' FROM '007')").contains('7'));
}

#[test]
fn expandarray_and_composite_field_access() {
    // `_pg_expandarray` (set-returning) + `(expression).n/.x` composite access,
    // driving JDBC getPrimaryKeys. A single-column PK expands to one row.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "CREATE TABLE pk1 (id int PRIMARY KEY, v text)",
    );
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "CREATE TABLE pk2 (a int, b int, PRIMARY KEY (a, b))",
    );
    // Single-column: one (x=1, n=1) row.
    let r1 = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT (information_schema._pg_expandarray(i.indkey)).n AS seq, \
         (information_schema._pg_expandarray(i.indkey)).x AS att \
         FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE c.relname = 'pk1_pkey'",
    ));
    assert_eq!(r1, ["1|1"], "single-col expand: {r1:?}");
    // Two-column PK: the SRF expands into two rows (ordinals 1 and 2). Sort
    // in a wrapping subquery, as JDBC's getPrimaryKeys does.
    let mut r2 = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT (information_schema._pg_expandarray(i.indkey)).n AS seq \
         FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE c.relname = 'pk2_pkey'",
    ));
    r2.sort();
    assert_eq!(r2, ["1", "2"], "multi-col expand: {r2:?}");
}

#[test]
fn regex_match_operators_and_operator_syntax() {
    // Semantics verified against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (s text)");
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "INSERT INTO t VALUES ('pg_toast'),('public'),('pg_temp_1'),('foo')",
    );
    // `~` and `!~` filter rows; `~*` is case-insensitive.
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut e,
            &mut b,
            &mut t,
            "SELECT s FROM t WHERE s ~ '^pg_' ORDER BY s"
        )),
        ["pg_temp_1", "pg_toast"]
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut e,
            &mut b,
            &mut t,
            "SELECT s FROM t WHERE s !~ '^pg_' ORDER BY s"
        )),
        ["foo", "public"]
    );
    assert!(run_txn(&mut e, &mut b, &mut t, "SELECT 'ABC' ~* '^abc'").contains('t'));
    assert!(run_txn(&mut e, &mut b, &mut t, "SELECT 'ABC' ~ '^abc'").contains('f'));
    // Grouping + alternation, and the explicit OPERATOR(...) syntax psql
    // emits, plus COLLATE (accepted, default collation).
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut e,
            &mut b,
            &mut t,
            "SELECT s FROM t WHERE s OPERATOR(pg_catalog.~) '^(foo|public)$' COLLATE \"C\" ORDER BY s"
        )),
        ["foo", "public"]
    );
    let unsupported = run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT 'a' COLLATE pg_catalog.pg_unicode_fast",
    );
    assert!(
        String::from_utf8_lossy(&unsupported).contains("0A000"),
        "{}",
        String::from_utf8_lossy(&unsupported)
    );
}

#[test]
fn window_functions() {
    // All outputs verified against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "CREATE TABLE s (dept text, name text, sal int)",
    );
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "INSERT INTO s VALUES ('a','w1',100),('a','w2',200),('a','w3',200),('b','w4',50),('b','w5',75)",
    );
    // row_number / rank / dense_rank with PARTITION BY + ORDER BY. Ranks
    // share for the tied 200/200 rows; row_number does not.
    let r = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT dept, sal, row_number() OVER (PARTITION BY dept ORDER BY sal, name), rank() OVER (PARTITION BY dept ORDER BY sal), dense_rank() OVER (PARTITION BY dept ORDER BY sal) FROM s ORDER BY dept, sal, name",
    ));
    assert_eq!(
        r,
        [
            "a|100|1|1|1",
            "a|200|2|2|2",
            "a|200|3|2|2",
            "b|50|1|1|1",
            "b|75|2|2|2"
        ],
        "rankings: {r:?}"
    );
    // Running sum (peers share) vs whole-partition sum.
    let s = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT sal, sum(sal) OVER (PARTITION BY dept ORDER BY sal), sum(sal) OVER (PARTITION BY dept) FROM s ORDER BY dept, sal, name",
    ));
    assert_eq!(
        s,
        [
            "100|100|500",
            "200|500|500",
            "200|500|500",
            "50|50|125",
            "75|125|125"
        ],
        "sums: {s:?}"
    );
    // lag / lead with a default.
    let l = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT sal, lag(sal) OVER (ORDER BY sal), lead(sal,1,-1) OVER (ORDER BY sal) FROM s ORDER BY sal",
    ));
    assert_eq!(
        l,
        [
            "50|NULL|75",
            "75|50|100",
            "100|75|200",
            "200|100|200",
            "200|200|-1"
        ],
        "lag/lead: {l:?}"
    );
}

#[test]
fn savepoints_rollback_and_release() {
    // Behavior verified against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int, v text)");
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (1,'a')");
    run_txn(&mut e, &mut b, &mut t, "SAVEPOINT s1");
    // Modify row 1 (touched before AND after the savepoint) and add row 2.
    run_txn(&mut e, &mut b, &mut t, "UPDATE t SET v='b' WHERE id=1");
    run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (2,'x')");
    // ROLLBACK TO restores row 1 to 'a' and removes row 2 — the reverse
    // replay reconstructs the savepoint-time image.
    assert!(run_txn(&mut e, &mut b, &mut t, "ROLLBACK TO SAVEPOINT s1").contains("ROLLBACK"));
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT id, v FROM t ORDER BY id",
    ));
    assert_eq!(rows, ["1|a"], "rollback to savepoint: {rows:?}");
    run_txn(&mut e, &mut b, &mut t, "COMMIT");
    // RELEASE keeps the subtransaction's changes.
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (3,'c')");
    run_txn(&mut e, &mut b, &mut t, "SAVEPOINT s2");
    run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (4,'d')");
    assert!(run_txn(&mut e, &mut b, &mut t, "RELEASE SAVEPOINT s2").contains("RELEASE"));
    run_txn(&mut e, &mut b, &mut t, "COMMIT");
    let all = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT id FROM t ORDER BY id",
    ));
    assert_eq!(all, ["1", "3", "4"], "release kept changes: {all:?}");
    // ROLLBACK TO recovers a failed subtransaction.
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    run_txn(&mut e, &mut b, &mut t, "SAVEPOINT s3");
    run_txn(&mut e, &mut b, &mut t, "SELECT 1/0");
    assert_eq!(t.status_byte(), b'E', "aborted after error");
    run_txn(&mut e, &mut b, &mut t, "ROLLBACK TO SAVEPOINT s3");
    assert_eq!(t.status_byte(), b'T', "recovered by rollback to savepoint");
    assert!(
        run_txn(&mut e, &mut b, &mut t, "SELECT 42").contains("42"),
        "works after recovery"
    );
    run_txn(&mut e, &mut b, &mut t, "COMMIT");
    // A nonexistent savepoint errors 3B001.
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    assert!(run_txn(&mut e, &mut b, &mut t, "ROLLBACK TO SAVEPOINT nope").contains("3B001"));
    run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
}

#[test]
fn update_from_and_delete_using() {
    // Row images verified against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "CREATE TABLE t (id int, v int, label text)",
    );
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "CREATE TABLE s (id int, delta int, lbl text)",
    );
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "INSERT INTO t VALUES (1,10,'x'),(2,20,'y'),(3,30,'z')",
    );
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "INSERT INTO s VALUES (1,100,'one'),(2,200,'two')",
    );
    // UPDATE ... FROM: the SET may reference both target and source columns.
    assert!(
        run_txn(
            &mut e,
            &mut b,
            &mut t,
            "UPDATE t SET v = t.v + s.delta, label = s.lbl FROM s WHERE t.id = s.id"
        )
        .contains("UPDATE 2")
    );
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT id, v, label FROM t ORDER BY id",
    ));
    assert_eq!(
        rows,
        ["1|110|one", "2|220|two", "3|30|z"],
        "update from: {rows:?}"
    );
    // DELETE ... USING removes the joined target rows.
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE d (id int, v int)");
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE k (id int)");
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "INSERT INTO d VALUES (1,1),(2,2),(3,3)",
    );
    run_txn(&mut e, &mut b, &mut t, "INSERT INTO k VALUES (2),(3)");
    assert!(
        run_txn(
            &mut e,
            &mut b,
            &mut t,
            "DELETE FROM d USING k WHERE d.id = k.id"
        )
        .contains("DELETE 2")
    );
    let left = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT id FROM d ORDER BY id",
    ));
    assert_eq!(left, ["1"], "delete using: {left:?}");
}

#[test]
fn multiway_equijoin_prunes_early() {
    // A chained k-way equi-join must push each equality down to the level
    // where its tables are bound and prune doomed partial rows there.
    // Without that this is a naive O(N^k) nested loop that never returns;
    // with it the test completes in milliseconds. Counts verified against
    // PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int, v int)");
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "INSERT INTO t SELECT g, g % 10 FROM generate_series(1, 80) g",
    );
    // Six-way self-join chained on a unique key: N distinct chains.
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT count(*) FROM t a, t b, t c, t d, t e, t f \
         WHERE a.id=b.id AND b.id=c.id AND c.id=d.id AND d.id=e.id AND e.id=f.id",
    ));
    assert_eq!(rows, ["80"], "6-way chained equi-join: {rows:?}");
    // A constant equality on a middle table prunes every chain but one.
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT count(*) FROM t a, t b, t c WHERE a.id=b.id AND b.id=c.id AND b.id=7",
    ));
    assert_eq!(rows, ["1"], "constant-pruned join: {rows:?}");
    // Pushdown must not change results: the leaf still checks the full WHERE,
    // so a non-key predicate that only the leaf can evaluate still filters.
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT count(*) FROM t a, t b WHERE a.id=b.id AND a.v + b.v = 4",
    ));
    assert_eq!(rows, ["8"], "leaf-checked predicate: {rows:?}");
}

#[test]
fn range_table_covers_wide_conformance_queries() {
    // The vendored SQLLogicTest select5 workload reaches seventeen relations
    // in one range table. This used to fail at a second, executor-only limit
    // even though the configured catalog could hold every relation.
    let (mut engine, mut budget) = test_engine();
    run_with(&mut engine, &mut budget, "CREATE TABLE t (id int)");
    run_with(&mut engine, &mut budget, "INSERT INTO t VALUES (1)");
    let rows = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "SELECT count(*) \
         FROM t a01, t a02, t a03, t a04, t a05, t a06, t a07, t a08, t a09, \
              t a10, t a11, t a12, t a13, t a14, t a15, t a16, t a17 \
         WHERE a01.id=a02.id AND a02.id=a03.id AND a03.id=a04.id \
           AND a04.id=a05.id AND a05.id=a06.id AND a06.id=a07.id \
           AND a07.id=a08.id AND a08.id=a09.id AND a09.id=a10.id \
           AND a10.id=a11.id AND a11.id=a12.id AND a12.id=a13.id \
           AND a13.id=a14.id AND a14.id=a15.id AND a15.id=a16.id \
           AND a16.id=a17.id",
    ));
    assert_eq!(rows, ["1"]);

    // Keep ample headroom above the seventeen-table conformance case while
    // exercising recursive execution on the test harness's constrained stack.
    let table_count = 32;
    let from = (1..=table_count)
        .map(|index| format!("t a{index:02}"))
        .collect::<Vec<_>>()
        .join(", ");
    let qualification = (1..table_count)
        .map(|index| format!("a{index:02}.id=a{:02}.id", index + 1))
        .collect::<Vec<_>>()
        .join(" AND ");
    let query = format!("SELECT count(*) FROM {from} WHERE {qualification}");
    assert_eq!(
        data_rows(&run_with(&mut engine, &mut budget, &query)),
        ["1"]
    );
}

#[test]
fn selective_join_component_precedes_independent_cross_filters() {
    // The final cardinality is small, but postponing the t1/t6 equality until
    // after the six independent filters creates more than sixteen million
    // nested-loop candidates. Equivalent FROM permutations must all establish
    // that selective component before multiplying it.
    let (mut e, mut b) = test_engine();
    for definition in [
        "CREATE TABLE t1(a1 int, d1 int)",
        "CREATE TABLE t2(d2 int)",
        "CREATE TABLE t3(a3 int)",
        "CREATE TABLE t4(b4 int)",
        "CREATE TABLE t5(c5 int)",
        "CREATE TABLE t6(b6 int, d6 int)",
        "CREATE TABLE t7(e7 int)",
        "CREATE TABLE t9(c9 int)",
    ] {
        run_with(&mut e, &mut b, definition);
    }
    for insert in [
        "INSERT INTO t1 SELECT g,g FROM generate_series(1,100) g",
        "INSERT INTO t2 SELECT g FROM generate_series(1,100) g",
        "INSERT INTO t3 SELECT g FROM generate_series(1,100) g",
        "INSERT INTO t4 SELECT g FROM generate_series(1,100) g",
        "INSERT INTO t5 SELECT g FROM generate_series(1,100) g",
        "INSERT INTO t6 SELECT g,100-g FROM generate_series(1,100) g",
        "INSERT INTO t7 SELECT g FROM generate_series(1,100) g",
        "INSERT INTO t9 SELECT g FROM generate_series(1,100) g",
    ] {
        run_with(&mut e, &mut b, insert);
    }
    let qualification = "t1.a1=t6.b6 AND t1.d1=t6.d6 \
        AND t2.d2=1 AND t4.b4 IN (1,2) AND t3.a3 IN (1,2,3) \
        AND t9.c9 IN (1,2,3,4,5) AND t7.e7 IN (1,2,3,4,5,6) \
        AND t5.c5 IN (1,2,3,4,5,6,7,8,9)";
    for from in [
        "t5,t2,t9,t6,t4,t1,t7,t3",
        "t6,t3,t9,t1,t7,t2,t4,t5",
        "t1,t4,t5,t7,t9,t3,t2,t6",
        "t5,t9,t1,t6,t3,t7,t2,t4",
    ] {
        let sql = format!(
            "SET statement_timeout=10000; SELECT count(*) FROM {from} WHERE {qualification}"
        );
        let rows = data_rows(&run_with(&mut e, &mut b, &sql));
        assert_eq!(rows, ["1620"], "FROM {from}");
    }
}

#[test]
fn named_timezone_dst_rendering() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
    // America/New_York: EST (-05) in winter, EDT (-04) in summer — DST honored.
    let out = run(
        "SET timezone='America/New_York'; SELECT '2021-01-15 12:00:00+00'::timestamptz, '2021-07-15 12:00:00+00'::timestamptz",
    );
    assert!(out.contains("07:00:00-05"), "winter EST: {out}");
    assert!(out.contains("08:00:00-04"), "summer EDT: {out}");
    // Southern hemisphere: DST in the local summer (January).
    let out = run("SET timezone='Australia/Sydney'; SELECT '2021-01-15 00:00:00+00'::timestamptz");
    assert!(out.contains("+11"), "AEDT: {out}");
    // An unknown zone is rejected loudly, not accepted.
    assert!(
        !run("SET timezone='Mars/Olympus'").contains("SET\0"),
        "unknown zone rejected"
    );
}

#[test]
fn commit_makes_writes_visible_and_durable() {
    let config = test_config("txn-durable");
    let mut b = Budget::new(1 << 25);
    {
        let mut e = Engine::new(&config, &mut b).unwrap();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int)");
        run_txn(
            &mut e,
            &mut b,
            &mut t,
            "BEGIN; INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); COMMIT",
        );
        run_txn(
            &mut e,
            &mut b,
            &mut t,
            "BEGIN; INSERT INTO t VALUES (3); ROLLBACK",
        );
        let slot = e.storage.find_table("public", "t").unwrap();
        let mut stamped = 0;
        e.storage
            .for_each_row_state(slot, &mut |_, state| {
                assert!(state.committed_lsn > 0);
                stamped += 1;
                Ok(core::ops::ControlFlow::Continue(()))
            })
            .unwrap();
        assert_eq!(stamped, 2);
    }
    let mut b2 = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut b2).unwrap();
    let mut t = TxnState::new(&mut b2, 256).unwrap();
    let out = run_txn(&mut e, &mut b2, &mut t, "SELECT id FROM t ORDER BY id");
    assert!(
        out.contains("SELECT 2"),
        "committed rows must replay: {out}"
    );
    assert!(!out.contains('3'), "rolled-back row must not replay: {out}");
    let slot = e.storage.find_table("public", "t").unwrap();
    e.storage
        .for_each_row_state(slot, &mut |_, state| {
            assert!(
                state.committed_lsn > 0,
                "WAL replay must retain commit LSNs"
            );
            Ok(core::ops::ControlFlow::Continue(()))
        })
        .unwrap();
}

#[test]
fn implicit_transaction_rolls_back_whole_message() {
    // An error in a multi-statement message undoes all of it.
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int)");
    let out = run_txn(
        &mut e,
        &mut b,
        &mut t,
        "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); SELECT 1/0",
    );
    assert!(out.contains("22012"), "{out}");
    let out = run_txn(&mut e, &mut b, &mut t, "SELECT count(*) FROM t");
    assert!(out.contains("count") || out.contains('0'), "{out}");
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT count(*) FROM t",
    ));
    assert_eq!(rows, ["0"], "inserts before the error must be undone");
}

/// Like `run_with`, but the caller supplies the session `GucState` so it
/// persists across statements — needed for session-scoped state (currval,
/// lastval), which a fresh `GucState` per call would reset.
fn run_session(
    engine: &mut Engine,
    budget: &mut Budget,
    guc: &mut GucState,
    sql_text: &str,
) -> Vec<u8> {
    let mut buffer = crate::mem::FixedBuf::new(budget, "send", 1 << 18).unwrap();
    let arena = Arena::new(budget, "sql", 1 << 18).unwrap();
    let mut txn = TxnState::new(budget, 1024).unwrap();
    let mut pool = test_pool(budget);
    let mut responder = Responder::new(&mut buffer);
    engine
        .execute_simple(
            sql_text,
            &arena,
            &mut txn,
            &mut pool,
            &mut test_cursors(budget),
            guc,
            &mut responder,
            1,
        )
        .unwrap();
    buffer.readable().to_vec()
}

fn run_session_transaction(
    engine: &mut Engine,
    budget: &mut Budget,
    transaction: &mut TxnState,
    guc: &mut GucState,
    sql_text: &str,
) -> Vec<u8> {
    let mut buffer = crate::mem::FixedBuf::new(budget, "send", 1 << 18).unwrap();
    let arena = Arena::new(budget, "sql", 1 << 18).unwrap();
    let mut pool = test_pool(budget);
    let mut responder = Responder::new(&mut buffer);
    engine
        .execute_simple(
            sql_text,
            &arena,
            transaction,
            &mut pool,
            &mut test_cursors(budget),
            guc,
            &mut responder,
            1,
        )
        .unwrap();
    buffer.readable().to_vec()
}

fn run_with_txn_bytes(
    engine: &mut Engine,
    budget: &mut Budget,
    txn: &mut TxnState,
    sql_text: &str,
) -> Vec<u8> {
    let mut buffer = crate::mem::FixedBuf::new(budget, "send", 1 << 18).unwrap();
    let arena = Arena::new(budget, "sql", 1 << 18).unwrap();
    let mut pool = test_pool(budget);
    let mut guc = GucState::new();
    let mut responder = Responder::new(&mut buffer);
    engine
        .execute_simple(
            sql_text,
            &arena,
            txn,
            &mut pool,
            &mut test_cursors(budget),
            &mut guc,
            &mut responder,
            1,
        )
        .unwrap();
    buffer.readable().to_vec()
}

#[test]
fn failed_transaction_blocks_until_end() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int)");
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (1)");
    let out = run_txn(&mut e, &mut b, &mut t, "SELECT 1/0");
    assert!(out.contains("22012"), "{out}");
    assert_eq!(t.status_byte(), b'E');
    let out = run_txn(&mut e, &mut b, &mut t, "SELECT 1");
    assert!(out.contains("25P02"), "{out}");
    // COMMIT of a failed txn reports ROLLBACK and undoes the insert.
    let out = run_txn(&mut e, &mut b, &mut t, "COMMIT");
    assert!(out.contains("ROLLBACK"), "{out}");
    assert_eq!(t.status_byte(), b'I');
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT count(*) FROM t",
    ));
    assert_eq!(rows, ["0"]);
}

#[test]
fn isolation_and_write_conflicts() {
    let (mut e, mut b) = test_engine();
    let mut alice = TxnState::new(&mut b, 256).unwrap();
    let mut bob = TxnState::new(&mut b, 256).unwrap();
    run_txn(
        &mut e,
        &mut b,
        &mut alice,
        "CREATE TABLE t (id int, v text)",
    );
    run_txn(
        &mut e,
        &mut b,
        &mut alice,
        "INSERT INTO t VALUES (1,'base')",
    );

    run_txn(&mut e, &mut b, &mut alice, "BEGIN");
    run_txn(
        &mut e,
        &mut b,
        &mut alice,
        "UPDATE t SET v = 'alice' WHERE id = 1",
    );
    run_txn(
        &mut e,
        &mut b,
        &mut alice,
        "INSERT INTO t VALUES (2,'alice-new')",
    );

    // Bob sees only committed state.
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut bob,
        "SELECT v FROM t ORDER BY id",
    ));
    assert_eq!(rows, ["base"], "uncommitted changes must be invisible");

    // Bob's writer parks without output until Alice releases the row.
    let out = run_txn(
        &mut e,
        &mut b,
        &mut bob,
        "UPDATE t SET v = 'bob' WHERE id = 1",
    );
    assert!(out.is_empty(), "a waiting writer emits nothing: {out}");

    run_txn(&mut e, &mut b, &mut alice, "COMMIT");
    let resumed = run_txn(
        &mut e,
        &mut b,
        &mut bob,
        "UPDATE t SET v = 'bob' WHERE id = 1",
    );
    assert!(resumed.contains("UPDATE 1"), "{resumed}");
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut bob,
        "SELECT v FROM t ORDER BY id",
    ));
    assert_eq!(rows, ["bob", "alice-new"]);
}

#[test]
fn ddl_rolls_back_with_implicit_transaction() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let out = run_txn(
        &mut e,
        &mut b,
        &mut t,
        "CREATE TABLE brand_new (id int); INSERT INTO brand_new VALUES (1); SELECT 1/0",
    );
    assert!(out.contains("22012"), "{out}");
    let out = run_txn(&mut e, &mut b, &mut t, "SELECT * FROM brand_new");
    assert!(
        out.contains("42P01"),
        "created table must be rolled back: {out}"
    );
    // DDL inside explicit blocks is transactional.
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE txn_ddl (id int)");
    run_txn(&mut e, &mut b, &mut t, "INSERT INTO txn_ddl VALUES (1)");
    run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
    let out = run_txn(&mut e, &mut b, &mut t, "SELECT * FROM txn_ddl");
    assert!(out.contains("42P01"), "{out}");
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE txn_ddl (id int)");
    run_txn(&mut e, &mut b, &mut t, "COMMIT");
    let out = run_txn(&mut e, &mut b, &mut t, "SELECT count(*) FROM txn_ddl");
    assert!(out.contains("count"), "{out}");
    // DROP rolls back too: the table and its rows survive.
    run_txn(&mut e, &mut b, &mut t, "INSERT INTO txn_ddl VALUES (7)");
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    run_txn(&mut e, &mut b, &mut t, "DROP TABLE txn_ddl");
    run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
    let rows = data_rows(&run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT id FROM txn_ddl",
    ));
    assert_eq!(rows, ["7"], "dropped table must revive with its rows");
    // CHECKPOINT stays outside transaction blocks.
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    let out = run_txn(&mut e, &mut b, &mut t, "CHECKPOINT");
    assert!(out.contains("0A000") || out.contains("25001"), "{out}");
    run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
}

#[test]
fn data_survives_engine_restart() {
    let config = test_config("restart");
    {
        let mut budget = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut budget).unwrap();
        run_with(&mut e, &mut budget, "CREATE TABLE t (id int, v text)");
        run_with(
            &mut e,
            &mut budget,
            "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')",
        );
        run_with(&mut e, &mut budget, "UPDATE t SET v = 'B' WHERE id = 2");
        run_with(&mut e, &mut budget, "DELETE FROM t WHERE id = 3");
        run_with(&mut e, &mut budget, "CREATE TABLE gone (x int)");
        run_with(&mut e, &mut budget, "DROP TABLE gone");
        e.commit_wal();
    }
    let mut budget = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut budget).unwrap();
    let bytes = run_with(&mut e, &mut budget, "SELECT id, v FROM t ORDER BY id");
    assert_eq!(data_rows(&bytes), ["1|a", "2|B"]);
    let bytes = run_with(&mut e, &mut budget, "SELECT * FROM gone");
    assert!(String::from_utf8_lossy(&bytes).contains("42P01"));
    // New rowids do not collide with replayed ones.
    run_with(&mut e, &mut budget, "INSERT INTO t VALUES (4,'d')");
    let bytes = run_with(&mut e, &mut budget, "SELECT id FROM t ORDER BY id");
    assert_eq!(data_rows(&bytes), ["1", "2", "4"]);
}

#[test]
fn indexes_survive_restart() {
    // Indexes (and their UNIQUE constraint) are journaled and survive a
    // WAL-replay restart.
    let config = test_config("idx_restart");
    {
        let mut budget = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut budget).unwrap();
        run_with(&mut e, &mut budget, "CREATE TABLE t (a int, b int)");
        run_with(&mut e, &mut budget, "INSERT INTO t VALUES (1,1),(1,2)");
        run_with(
            &mut e,
            &mut budget,
            "CREATE UNIQUE INDEX u ON t(a DESC NULLS LAST,b ASC NULLS FIRST)",
        );
        e.commit_wal();
    }
    let mut budget = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut budget).unwrap();
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut budget,
            "SELECT indexdef FROM pg_indexes WHERE indexname = 'u'"
        )),
        ["CREATE UNIQUE INDEX u ON public.t USING btree (a DESC NULLS LAST, b NULLS FIRST)"]
    );
    // The UNIQUE index survived: a conflicting insert is rejected.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut budget, "INSERT INTO t VALUES (1,1)"))
            .contains("23505")
    );
    // A non-conflicting insert works.
    let out = String::from_utf8_lossy(&run_with(&mut e, &mut budget, "INSERT INTO t VALUES (3,3)"))
        .to_string();
    assert!(!out.contains("23505"), "{out}");
}

#[test]
fn views_survive_restart() {
    // View definitions are journaled, so they survive a WAL-replay restart.
    let config = test_config("view_restart");
    {
        let mut budget = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut budget).unwrap();
        run_with(&mut e, &mut budget, "CREATE TABLE t (id int, v int)");
        run_with(
            &mut e,
            &mut budget,
            "INSERT INTO t VALUES (1,10),(2,20),(3,30)",
        );
        run_with(
            &mut e,
            &mut budget,
            "CREATE VIEW big AS SELECT id FROM t WHERE v > 15",
        );
        run_with(&mut e, &mut budget, "CREATE VIEW gone AS SELECT 1");
        run_with(&mut e, &mut budget, "DROP VIEW gone");
        e.commit_wal();
    }
    let mut budget = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut budget).unwrap();
    // The surviving view still expands and queries.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut budget,
            "SELECT id FROM big ORDER BY id"
        )),
        ["2", "3"]
    );
    // The dropped view is gone.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut budget, "SELECT * FROM gone"))
            .contains("42P01")
    );
}

#[test]
fn matview_survives_restart() {
    // A materialized view's rows (its backing table) and its defining query
    // (the matview catalog) are both journaled, so they survive a WAL-replay
    // restart — and REFRESH still works afterward.
    let config = test_config("matview_restart");
    {
        let mut budget = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut budget).unwrap();
        run_with(&mut e, &mut budget, "CREATE TABLE t (id int, v int)");
        run_with(
            &mut e,
            &mut budget,
            "INSERT INTO t VALUES (1,10),(2,20),(3,30)",
        );
        run_with(
            &mut e,
            &mut budget,
            "CREATE MATERIALIZED VIEW mv AS SELECT id FROM t WHERE v > 15",
        );
        e.commit_wal();
    }
    let mut budget = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut budget).unwrap();
    // The materialized rows survived the restart.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut budget,
            "SELECT id FROM mv ORDER BY id"
        )),
        ["2", "3"]
    );
    // It is reported as a materialized view (relkind 'm'), not a table.
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut budget,
            "SELECT relkind FROM pg_class WHERE relname = 'mv'"
        ))
        .contains('m')
    );
    // DROP TABLE is refused (42809).
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut budget, "DROP TABLE mv")).contains("42809")
    );
    // A base change is invisible until REFRESH, which the stored query drives.
    run_with(&mut e, &mut budget, "INSERT INTO t VALUES (4,40)");
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut budget,
            "SELECT id FROM mv ORDER BY id"
        )),
        ["2", "3"]
    );
    run_with(&mut e, &mut budget, "REFRESH MATERIALIZED VIEW mv");
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut budget,
            "SELECT id FROM mv ORDER BY id"
        )),
        ["2", "3", "4"]
    );
    // DROP MATERIALIZED VIEW removes it.
    run_with(&mut e, &mut budget, "DROP MATERIALIZED VIEW mv");
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut budget, "SELECT * FROM mv"))
            .contains("42P01")
    );
}

#[test]
fn sequence_basics() {
    let (mut e, mut b) = test_engine();
    // A persistent session GucState so currval/lastval survive across statements.
    let mut g = GucState::new();
    run_session(
        &mut e,
        &mut b,
        &mut g,
        "CREATE SEQUENCE s START 5 INCREMENT 2",
    );
    assert_eq!(
        data_rows(&run_session(&mut e, &mut b, &mut g, "SELECT nextval('s')")),
        ["5"]
    );
    assert_eq!(
        data_rows(&run_session(&mut e, &mut b, &mut g, "SELECT nextval('s')")),
        ["7"]
    );
    assert_eq!(
        data_rows(&run_session(&mut e, &mut b, &mut g, "SELECT currval('s')")),
        ["7"]
    );
    assert_eq!(
        data_rows(&run_session(&mut e, &mut b, &mut g, "SELECT lastval()")),
        ["7"]
    );
    assert_eq!(
        data_rows(&run_session(
            &mut e,
            &mut b,
            &mut g,
            "SELECT setval('s', 100)"
        )),
        ["100"]
    );
    assert_eq!(
        data_rows(&run_session(&mut e, &mut b, &mut g, "SELECT currval('s')")),
        ["100"]
    );
    assert_eq!(
        data_rows(&run_session(&mut e, &mut b, &mut g, "SELECT nextval('s')")),
        ["102"]
    );
    // relkind 'S' in pg_class; pg_sequences lists it.
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "SELECT relkind FROM pg_class WHERE relname='s'"
        ))
        .contains('S')
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT last_value FROM pg_sequences WHERE sequencename='s'"
        )),
        ["102"]
    );
    // INSERT ... VALUES(nextval) advances once per row.
    run_with(&mut e, &mut b, "CREATE TABLE t (id bigint)");
    run_with(&mut e, &mut b, "CREATE SEQUENCE q");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO t VALUES (nextval('q')), (nextval('q')), (nextval('q'))",
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT id FROM t ORDER BY id")),
        ["1", "2", "3"]
    );
    // UPDATE SET = nextval advances once per updated row.
    run_with(&mut e, &mut b, "UPDATE t SET id = nextval('q')");
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT id FROM t ORDER BY id")),
        ["4", "5", "6"]
    );
    // currval is undefined before the first nextval in a session (55000).
    run_with(&mut e, &mut b, "CREATE SEQUENCE u");
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT currval('u')")).contains("55000")
    );
    // Overflow on a non-cycling sequence (2200H).
    run_with(&mut e, &mut b, "CREATE SEQUENCE o MAXVALUE 2 NO CYCLE");
    run_with(&mut e, &mut b, "SELECT nextval('o')");
    run_with(&mut e, &mut b, "SELECT nextval('o')");
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT nextval('o')")).contains("2200H")
    );
    // DROP SEQUENCE removes it.
    run_with(&mut e, &mut b, "DROP SEQUENCE s");
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT nextval('s')")).contains("42P01")
    );
}

#[test]
fn sequence_survives_restart() {
    let config = test_config("sequence_restart");
    {
        let mut budget = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut budget).unwrap();
        run_with(
            &mut e,
            &mut budget,
            "CREATE SEQUENCE s START 10 INCREMENT 5 MAXVALUE 100 CYCLE",
        );
        run_with(&mut e, &mut budget, "SELECT nextval('s')"); // 10
        run_with(&mut e, &mut budget, "SELECT nextval('s')"); // 15
        e.commit_wal();
    }
    let mut budget = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut budget).unwrap();
    // Value state (last=15, is_called) survived replay: the next value is 20.
    assert_eq!(
        data_rows(&run_with(&mut e, &mut budget, "SELECT nextval('s')")),
        ["20"]
    );
    // Parameters survived too (increment 5, cycle).
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut budget,
            "SELECT increment_by, cycle FROM pg_sequences WHERE sequencename='s'"
        )),
        ["5|t"]
    );
}

#[test]
fn sequence_advance_in_creating_transaction_survives_restart() {
    let config = test_config("sequence_create_advance_restart");
    {
        let mut budget = Budget::new(1 << 25);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let result = run_with(
            &mut engine,
            &mut budget,
            "BEGIN; \
             CREATE SEQUENCE s START 10 INCREMENT 5; \
             SELECT nextval('s'); \
             CREATE TABLE generated (id serial PRIMARY KEY); \
             INSERT INTO generated DEFAULT VALUES; \
             COMMIT",
        );
        assert!(!String::from_utf8_lossy(&result).contains("ERROR"));
    }

    let mut budget = Budget::new(1 << 25);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    assert_eq!(
        data_rows(&run_with(&mut engine, &mut budget, "SELECT nextval('s')")),
        ["15"]
    );
    run_with(
        &mut engine,
        &mut budget,
        "INSERT INTO generated DEFAULT VALUES",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT id FROM generated ORDER BY id"
        )),
        ["1", "2"]
    );
}

#[test]
fn journal_full_keeps_sequence_advance_dirty_for_retry() {
    let mut config = test_config("sequence_journal_retry");
    config.wal_bytes = 120;
    let mut budget = Budget::new(1 << 25);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let created = run_with(&mut engine, &mut budget, "CREATE SEQUENCE s");
    assert!(
        !String::from_utf8_lossy(&created).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&created)
    );
    let slot = engine.storage.sequence_slot("public", "s", 0).unwrap();
    assert!(!engine.storage.sequence(slot).dirty.get());

    let failed = run_with(&mut engine, &mut budget, "SELECT nextval('s')");
    assert!(String::from_utf8_lossy(&failed).contains("53100"));
    assert_eq!(engine.storage.sequence(slot).last_value.get(), 1);
    assert!(engine.storage.sequence(slot).dirty.get());

    // Model the journal space a successful checkpoint makes available. A
    // later read-only commit must retry the absolute sequence position.
    engine.wal.reset_after_checkpoint();
    let retried = run_with(&mut engine, &mut budget, "SELECT 1");
    assert!(!String::from_utf8_lossy(&retried).contains("ERROR"));
    assert!(!engine.storage.sequence(slot).dirty.get());
}

#[test]
fn expression_defaults() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE SEQUENCE dseq");
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE d (id bigint DEFAULT nextval('dseq'), n int DEFAULT 2 + 3, note text DEFAULT 'x')",
    );
    // A per-row DEFAULT nextval advances once per inserted row; a constant
    // default folds; an explicit value does not advance the sequence.
    run_with(&mut e, &mut b, "INSERT INTO d (note) VALUES ('a'), ('b')");
    run_with(&mut e, &mut b, "INSERT INTO d DEFAULT VALUES");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO d (id, note) VALUES (100, 'explicit')",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT id, n, note FROM d ORDER BY id"
        )),
        ["1|5|a", "2|5|b", "3|5|x", "100|5|explicit"]
    );
    // DEFAULT now() is evaluated per insert, not folded to CREATE-TABLE time:
    // two rows inserted after a fresh statement clock share the statement's now.
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE t (ts timestamptz DEFAULT now(), v int)",
    );
    run_with(&mut e, &mut b, "INSERT INTO t (v) VALUES (1)");
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT count(*) FROM t WHERE ts IS NOT NULL"
        )),
        ["1"]
    );
    // ALTER COLUMN SET DEFAULT with an expression, then ADD COLUMN with one.
    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE t ALTER COLUMN v SET DEFAULT nextval('dseq')",
    );
    run_with(&mut e, &mut b, "INSERT INTO t (v) VALUES (DEFAULT)");
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT v FROM t ORDER BY v")),
        ["1", "4"]
    );
}

#[test]
fn expression_default_survives_restart() {
    let config = test_config("default_expr_restart");
    {
        let mut budget = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut budget).unwrap();
        run_with(&mut e, &mut budget, "CREATE SEQUENCE s");
        run_with(
            &mut e,
            &mut budget,
            "CREATE TABLE t (id bigint DEFAULT nextval('s'), v int)",
        );
        run_with(&mut e, &mut budget, "INSERT INTO t (v) VALUES (10)");
        e.commit_wal();
    }
    let mut budget = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut budget).unwrap();
    // The default expression survived replay: the next insert still assigns
    // nextval (continuing the sequence).
    run_with(&mut e, &mut budget, "INSERT INTO t (v) VALUES (20)");
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut budget,
            "SELECT id, v FROM t ORDER BY v"
        )),
        ["1|10", "2|20"]
    );
}

#[test]
fn generated_columns() {
    let (mut e, mut b) = test_engine();
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE g (a int, b int, c int GENERATED ALWAYS AS (a + b) STORED, \
         label text GENERATED ALWAYS AS (a::text || '-' || b::text) STORED)",
    );
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO g (a, b) VALUES (2, 3), (10, 20)",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT a, b, c, label FROM g ORDER BY a"
        )),
        ["2|3|5|2-3", "10|20|30|10-20"]
    );
    // Cannot insert a non-DEFAULT value into a generated column (428C9).
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "INSERT INTO g (a, b, c) VALUES (1, 1, 5)"
        ))
        .contains("428C9")
    );
    // DEFAULT is allowed and recomputes.
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO g (a, b, c) VALUES (1, 1, DEFAULT)",
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT c FROM g WHERE a = 1")),
        ["2"]
    );
    // UPDATE of a dependency recomputes; direct update rejected except DEFAULT.
    run_with(&mut e, &mut b, "UPDATE g SET b = 100 WHERE a = 2");
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT c FROM g WHERE a = 2")),
        ["102"]
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "UPDATE g SET c = 99 WHERE a = 10"
        ))
        .contains("428C9")
    );
    run_with(&mut e, &mut b, "UPDATE g SET c = DEFAULT WHERE a = 10");
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT c FROM g WHERE a = 10")),
        ["30"]
    );
    // attgenerated is 's'.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT attgenerated FROM pg_attribute WHERE attrelid='g'::regclass AND attname='c'"
        )),
        ["s"]
    );
    // Restrictions.
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "CREATE TABLE bad1 (a int, x int GENERATED ALWAYS AS (now()) STORED)"
        ))
        .contains("42P17")
    );
    assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "CREATE TABLE bad2 (a int, x int GENERATED ALWAYS AS (a) STORED, y int GENERATED ALWAYS AS (x) STORED)"))
        .contains("42P17"));
    // ADD COLUMN generated backfills existing rows.
    run_with(&mut e, &mut b, "CREATE TABLE h (a int)");
    run_with(&mut e, &mut b, "INSERT INTO h VALUES (5), (7)");
    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE h ADD COLUMN d int GENERATED ALWAYS AS (a * 10) STORED",
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT a, d FROM h ORDER BY a")),
        ["5|50", "7|70"]
    );
}

#[test]
fn generated_column_survives_restart() {
    let config = test_config("generated_restart");
    {
        let mut budget = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut budget).unwrap();
        run_with(
            &mut e,
            &mut budget,
            "CREATE TABLE g (a int, c int GENERATED ALWAYS AS (a + 1) STORED)",
        );
        run_with(&mut e, &mut budget, "INSERT INTO g (a) VALUES (10)");
        e.commit_wal();
    }
    let mut budget = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut budget).unwrap();
    // The generation expression survived replay: a new insert still computes it.
    run_with(&mut e, &mut budget, "INSERT INTO g (a) VALUES (20)");
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut budget,
            "SELECT a, c FROM g ORDER BY a"
        )),
        ["10|11", "20|21"]
    );
}

#[test]
fn identity_columns() {
    let (mut e, mut b) = test_engine();
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE ia (id int GENERATED ALWAYS AS IDENTITY, v text)",
    );
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE ib (id int GENERATED BY DEFAULT AS IDENTITY, v text)",
    );
    run_with(&mut e, &mut b, "INSERT INTO ia (v) VALUES ('a'), ('b')");
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT id, v FROM ia ORDER BY id"
        )),
        ["1|a", "2|b"]
    );
    // ALWAYS rejects an explicit value (428C9) unless OVERRIDING SYSTEM VALUE.
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "INSERT INTO ia (id, v) VALUES (100, 'x')"
        ))
        .contains("428C9")
    );
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO ia (id, v) OVERRIDING SYSTEM VALUE VALUES (100, 'x')",
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT id FROM ia ORDER BY id")),
        ["1", "2", "100"]
    );
    // BY DEFAULT accepts an explicit value; OVERRIDING USER VALUE ignores it.
    run_with(&mut e, &mut b, "INSERT INTO ib (id, v) VALUES (50, 'y')");
    run_with(&mut e, &mut b, "INSERT INTO ib (v) VALUES ('z')");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO ib (id, v) OVERRIDING USER VALUE VALUES (999, 'uv')",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT id, v FROM ib ORDER BY id"
        )),
        ["1|z", "2|uv", "50|y"]
    );
    // attidentity 'a' / 'd'.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT attidentity FROM pg_attribute WHERE attrelid='ia'::regclass AND attname='id'"
        )),
        ["a"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT attidentity FROM pg_attribute WHERE attrelid='ib'::regclass AND attname='id'"
        )),
        ["d"]
    );
    // Identity with START/INCREMENT options.
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE ic (id int GENERATED ALWAYS AS IDENTITY (START WITH 10 INCREMENT BY 5), v text)",
    );
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO ic (v) VALUES ('a'), ('b'), ('c')",
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT id FROM ic ORDER BY id")),
        ["10", "15", "20"]
    );
    // ALTER ADD IDENTITY requires NOT NULL; DROP IDENTITY removes it.
    run_with(&mut e, &mut b, "CREATE TABLE id2 (id int NOT NULL, v text)");
    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE id2 ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY",
    );
    run_with(&mut e, &mut b, "INSERT INTO id2 (v) VALUES ('a')");
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT id, v FROM id2")),
        ["1|a"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT attidentity FROM pg_attribute WHERE attrelid='id2'::regclass AND attname='id'"
        )),
        ["a"]
    );
    // ADD IDENTITY on a nullable column is 55000.
    run_with(&mut e, &mut b, "CREATE TABLE id3 (id int, v text)");
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "ALTER TABLE id3 ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY"
        ))
        .contains("55000")
    );
    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE id2 ALTER COLUMN id DROP IDENTITY",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT attidentity FROM pg_attribute WHERE attrelid='id2'::regclass AND attname='id'"
        )),
        [""]
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT nextval('id2_id_seq')"))
            .contains("42P01"),
        "DROP IDENTITY drops its owned sequence"
    );

    // PostgreSQL 18's plain dump spelling: DEFAULT before NOT NULL, ALTER
    // TABLE ONLY, a custom identity sequence name with the full option set,
    // and an explicit btree access method.
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE dump_parent (id int PRIMARY KEY);
         CREATE TABLE dump_entry (
           id bigint NOT NULL,
           parent_id int,
           state text DEFAULT 'ok'::text NOT NULL
         );
         ALTER TABLE dump_entry ALTER COLUMN id ADD GENERATED BY DEFAULT AS IDENTITY (
           SEQUENCE NAME dump_entry_custom_seq START WITH 1 INCREMENT BY 1
           NO MINVALUE NO MAXVALUE CACHE 1 NO CYCLE
         );
         ALTER TABLE ONLY dump_entry ADD CONSTRAINT dump_entry_parent_fkey
           FOREIGN KEY (parent_id) REFERENCES dump_parent(id);
         CREATE INDEX dump_entry_state_idx ON dump_entry USING btree (state);
         SELECT setval('dump_entry_custom_seq', 2, true);",
    );
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO dump_entry(parent_id) VALUES (NULL)",
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT id,state FROM dump_entry")),
        ["3|ok"]
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "DROP SEQUENCE dump_entry_custom_seq"
        ))
        .contains("2BP01"),
        "an owned identity sequence cannot be dropped independently"
    );
}

#[test]
fn sequence_ownership_is_distinct_from_generation() {
    let (mut e, mut b) = test_engine();
    // Ownership is a lifecycle dependency, not the generator selection. An
    // unrelated sequence may own the same column without stealing identity
    // values or being dropped by DROP IDENTITY.
    let owner_setup = run_with(
        &mut e,
        &mut b,
        "CREATE TABLE owner_split (id integer NOT NULL, payload text);
         CREATE SEQUENCE owner_split_extra START WITH 100 OWNED BY owner_split.id;
         ALTER TABLE owner_split ALTER COLUMN id ADD GENERATED BY DEFAULT AS IDENTITY (
           SEQUENCE NAME owner_split_generator START WITH 10
         );",
    );
    assert!(
        String::from_utf8_lossy(&owner_setup).contains("ALTER TABLE"),
        "{}",
        String::from_utf8_lossy(&owner_setup)
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "INSERT INTO owner_split(payload) VALUES ('x') RETURNING id"
        )),
        ["10"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT nextval('owner_split_extra')"
        )),
        ["100"]
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "ALTER SEQUENCE owner_split_generator OWNED BY NONE"
        ))
        .contains("0A000"),
        "PostgreSQL forbids changing an identity sequence's ownership"
    );
    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE owner_split ALTER COLUMN id DROP IDENTITY",
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "SELECT nextval('owner_split_generator')"
        ))
        .contains("42P01")
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT nextval('owner_split_extra')"
        )),
        ["101"]
    );
    run_with(&mut e, &mut b, "DROP TABLE owner_split");
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "SELECT nextval('owner_split_extra')"
        ))
        .contains("42P01")
    );

    // Serial's nextval default remains after OWNED BY NONE, while the detached
    // sequence correctly survives DROP TABLE.
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE detached_serial (id serial, payload text);
         ALTER SEQUENCE detached_serial_id_seq OWNED BY NONE;",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "INSERT INTO detached_serial(payload) VALUES ('x') RETURNING id"
        )),
        ["1"]
    );
    run_with(&mut e, &mut b, "DROP TABLE detached_serial");
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT nextval('detached_serial_id_seq')"
        )),
        ["2"]
    );

    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE owned_drop (id integer, keep integer);
         CREATE SEQUENCE owned_drop_a OWNED BY owned_drop.id;
         CREATE SEQUENCE owned_drop_b OWNED BY owned_drop.id;
         ALTER TABLE owned_drop DROP COLUMN id;",
    );
    for sequence in ["owned_drop_a", "owned_drop_b"] {
        assert!(
            String::from_utf8_lossy(&run_with(
                &mut e,
                &mut b,
                &format!("SELECT nextval('{sequence}')")
            ))
            .contains("42P01"),
            "DROP COLUMN must drop every sequence it owns"
        );
    }
}

#[test]
fn identity_survives_restart() {
    let config = test_config("identity_restart");
    {
        let mut budget = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut budget).unwrap();
        run_with(
            &mut e,
            &mut budget,
            "CREATE TABLE ic (id int GENERATED ALWAYS AS IDENTITY (START WITH 10 INCREMENT BY 5), v text)",
        );
        run_with(
            &mut e,
            &mut budget,
            "CREATE SEQUENCE ic_v_seq OWNED BY ic.v",
        );
        run_with(&mut e, &mut budget, "INSERT INTO ic (v) VALUES ('a')");
        run_with(
            &mut e,
            &mut budget,
            "ALTER TABLE ic RENAME COLUMN id TO key",
        );
        run_with(
            &mut e,
            &mut budget,
            "ALTER TABLE ic RENAME COLUMN v TO value",
        );
        run_with(&mut e, &mut budget, "ALTER TABLE ic RENAME TO ic2");
        e.commit_wal();
    }
    let mut budget = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut budget).unwrap();
    // The identity step (5) and counter survived replay: next value is 15.
    run_with(&mut e, &mut budget, "INSERT INTO ic2 (value) VALUES ('b')");
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut budget,
            "SELECT key, value FROM ic2 ORDER BY key"
        )),
        ["10|a", "15|b"]
    );
    run_with(&mut e, &mut budget, "DROP TABLE ic2");
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut budget, "SELECT nextval('ic_v_seq')"))
            .contains("42P01"),
        "a renamed ordinary OWNED BY dependency survives WAL replay"
    );
}

#[test]
fn merge_statement() {
    let (mut e, mut b) = test_engine();
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE tgt (id int PRIMARY KEY, v text, n int)",
    );
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO tgt VALUES (1,'a',10),(2,'b',20),(3,'c',30)",
    );
    run_with(&mut e, &mut b, "CREATE TABLE src (id int, v text)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO src VALUES (2,'B'),(3,'C'),(4,'D'),(5,'E')",
    );
    let out = run_with(
        &mut e,
        &mut b,
        "MERGE INTO tgt t USING src s ON t.id = s.id \
         WHEN MATCHED AND s.id = 3 THEN DELETE \
         WHEN MATCHED THEN UPDATE SET v = s.v, n = t.n + 1 \
         WHEN NOT MATCHED AND s.id = 5 THEN DO NOTHING \
         WHEN NOT MATCHED THEN INSERT (id, v, n) VALUES (s.id, s.v, 0)",
    );
    assert!(String::from_utf8_lossy(&out).contains("MERGE 3"));
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT id, v, n FROM tgt ORDER BY id"
        )),
        ["1|a|10", "2|B|21", "4|D|0"]
    );
    // Cardinality: a target row matched by two source rows → 21000.
    run_with(&mut e, &mut b, "INSERT INTO src VALUES (2,'dup')");
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "MERGE INTO tgt t USING src s ON t.id=s.id WHEN MATCHED THEN UPDATE SET v=s.v"
        ))
        .contains("21000")
    );
    // VALUES source.
    run_with(
        &mut e,
        &mut b,
        "MERGE INTO tgt t USING (VALUES (10,'x')) s(id,v) ON t.id=s.id WHEN NOT MATCHED THEN INSERT (id,v,n) VALUES (s.id, s.v, 99)",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT v, n FROM tgt WHERE id=10"
        )),
        ["x|99"]
    );
}

#[test]
fn sql_surface_batch() {
    let (mut e, mut b) = test_engine();
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE s (id int, name text DEFAULT 'x', qty int DEFAULT 3)",
    );
    let bytes = run_with(
        &mut e,
        &mut b,
        "INSERT INTO s (id) VALUES (1), (2) RETURNING id, name, qty",
    );
    assert_eq!(data_rows(&bytes), ["1|x|3", "2|x|3"]);
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO s VALUES (3, DEFAULT, 9), (4, 'y', 1)",
    );

    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT id FROM s WHERE id IN (2,4) ORDER BY 1",
    );
    assert_eq!(data_rows(&bytes), ["2", "4"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT id FROM s WHERE qty BETWEEN 2 AND 5 ORDER BY id",
    );
    assert_eq!(data_rows(&bytes), ["1", "2"]);
    let bytes = run_with(&mut e, &mut b, "SELECT DISTINCT name FROM s ORDER BY name");
    assert_eq!(data_rows(&bytes), ["x", "y"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT id FROM s ORDER BY id OFFSET 1 LIMIT 2",
    );
    assert_eq!(data_rows(&bytes), ["2", "3"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT CASE WHEN qty > 5 THEN 'hi' ELSE 'lo' END FROM s ORDER BY id",
    );
    assert_eq!(data_rows(&bytes), ["lo", "lo", "hi", "lo"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT name FROM s WHERE name LIKE '_' AND name NOT LIKE 'x' ORDER BY id LIMIT 1",
    );
    assert_eq!(data_rows(&bytes), ["y"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "UPDATE s SET qty = 0 WHERE id = 4 RETURNING qty",
    );
    assert_eq!(data_rows(&bytes), ["0"]);
    let bytes = run_with(&mut e, &mut b, "DELETE FROM s WHERE id = 1 RETURNING name");
    assert_eq!(data_rows(&bytes), ["x"]);

    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE s ADD COLUMN price float8 DEFAULT 1.5",
    );
    run_with(&mut e, &mut b, "ALTER TABLE s RENAME COLUMN name TO title");
    run_with(&mut e, &mut b, "ALTER TABLE s DROP COLUMN qty");
    run_with(&mut e, &mut b, "ALTER TABLE s RENAME TO stock");
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT id, title, price FROM stock ORDER BY id",
    );
    assert_eq!(data_rows(&bytes), ["2|x|1.5", "3|x|1.5", "4|y|1.5"]);

    // The pool is per-connection; one message keeps one pool here.
    let bytes = run_with(
        &mut e,
        &mut b,
        "PREPARE q (int) AS SELECT title FROM stock WHERE id = $1; EXECUTE q(4); \
         DEALLOCATE q; EXECUTE q(4)",
    );
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert_eq!(data_rows(&bytes), ["y"], "{text}");
    assert!(text.contains("26000"), "{text}");
}

#[test]
fn altered_table_survives_restart() {
    let config = test_config("alter-durable");
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        run_with(&mut e, &mut b, "CREATE TABLE a (id int, v text)");
        run_with(&mut e, &mut b, "CREATE INDEX a_v_idx ON a (v)");
        run_with(&mut e, &mut b, "INSERT INTO a VALUES (1, 'one')");
        run_with(&mut e, &mut b, "ALTER TABLE a ADD COLUMN n int DEFAULT 42");
        run_with(&mut e, &mut b, "ALTER TABLE a RENAME TO b");
    }
    let mut b = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut b).unwrap();
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT id, v, n FROM b;
         SELECT tablename FROM pg_indexes WHERE indexname = 'a_v_idx';",
    );
    assert_eq!(data_rows(&bytes), ["1|one|42", "b"]);
}

#[test]
fn alter_column_default_and_not_null() {
    let config = test_config("alter-column");
    let mut b = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut b).unwrap();
    run_with(&mut e, &mut b, "CREATE TABLE ac (id int, a int, b text)");
    run_with(&mut e, &mut b, "INSERT INTO ac VALUES (1, NULL, 'x')");
    // SET DEFAULT (COLUMN keyword optional) fills omitted columns on INSERT.
    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE ac ALTER COLUMN b SET DEFAULT 'd'",
    );
    run_with(&mut e, &mut b, "ALTER TABLE ac ALTER a SET DEFAULT 7");
    let bytes = run_with(
        &mut e,
        &mut b,
        "INSERT INTO ac (id) VALUES (2) RETURNING a, b",
    );
    assert_eq!(data_rows(&bytes), ["7|d"]);
    // DROP DEFAULT: the column falls back to NULL.
    run_with(&mut e, &mut b, "ALTER TABLE ac ALTER COLUMN a DROP DEFAULT");
    let bytes = run_with(
        &mut e,
        &mut b,
        "INSERT INTO ac (id) VALUES (3) RETURNING coalesce(a, -1)",
    );
    assert_eq!(data_rows(&bytes), ["-1"]);
    // SET NOT NULL is refused while a NULL is present (23502).
    let bytes = run_with(&mut e, &mut b, "ALTER TABLE ac ALTER COLUMN a SET NOT NULL");
    assert!(
        String::from_utf8_lossy(&bytes).contains("23502"),
        "expected not-null violation"
    );
    run_with(&mut e, &mut b, "UPDATE ac SET a = 0 WHERE a IS NULL");
    run_with(&mut e, &mut b, "ALTER TABLE ac ALTER COLUMN a SET NOT NULL");
    // Now enforced on new inserts.
    let bytes = run_with(&mut e, &mut b, "INSERT INTO ac (id, b) VALUES (4, 'z')");
    assert!(
        String::from_utf8_lossy(&bytes).contains("23502"),
        "expected enforcement"
    );
    // DROP NOT NULL lifts it; a NULL then inserts.
    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE ac ALTER COLUMN a DROP NOT NULL",
    );
    run_with(&mut e, &mut b, "INSERT INTO ac (id, b) VALUES (5, 'w')");
    // Rows 1,2,3,5 — the id=4 insert was rejected above, so four remain.
    let bytes = run_with(&mut e, &mut b, "SELECT count(*) FROM ac");
    assert_eq!(data_rows(&bytes), ["4"]);
}

#[test]
fn alter_column_type_rewrites_and_persists() {
    let config = test_config("alter-column-type");
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        run_with(&mut e, &mut b, "CREATE TABLE ct (id int, a int, b text)");
        run_with(
            &mut e,
            &mut b,
            "INSERT INTO ct VALUES (1, 42, 'hello'), (2, 100, 'yo')",
        );
        // Assignment cast (int -> text) needs no USING.
        run_with(&mut e, &mut b, "ALTER TABLE ct ALTER COLUMN a TYPE text");
        // Explicit-only cast without USING is refused (42804).
        let bytes = run_with(&mut e, &mut b, "ALTER TABLE ct ALTER COLUMN b TYPE int");
        assert!(
            String::from_utf8_lossy(&bytes).contains("42804"),
            "expected cast-automatically error"
        );
        // USING evaluates over the old row.
        run_with(
            &mut e,
            &mut b,
            "ALTER TABLE ct ALTER COLUMN b TYPE int USING length(b)",
        );
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT a, pg_typeof(a), b FROM ct ORDER BY id",
        );
        assert_eq!(data_rows(&bytes), ["42|text|5", "100|text|2"]);
    }
    // The rewritten shape and values survive a restart.
    let mut b = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut b).unwrap();
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT a, pg_typeof(b), b FROM ct ORDER BY id",
    );
    assert_eq!(data_rows(&bytes), ["42|integer|5", "100|integer|2"]);
}

#[test]
fn alter_add_drop_constraint() {
    let config = test_config("alter-constraint");
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        run_with(&mut e, &mut b, "CREATE TABLE ch (id int, a int, b int)");
        run_with(
            &mut e,
            &mut b,
            "INSERT INTO ch VALUES (1, 5, 10), (2, 7, 20)",
        );
        // ADD CHECK: violated by an existing row is refused (23514).
        let bytes = run_with(
            &mut e,
            &mut b,
            "ALTER TABLE ch ADD CONSTRAINT ck CHECK (a > 6)",
        );
        assert!(
            String::from_utf8_lossy(&bytes).contains("23514"),
            "expected check violation on add"
        );
        // A satisfied one attaches and is then enforced.
        run_with(
            &mut e,
            &mut b,
            "ALTER TABLE ch ADD CONSTRAINT ck CHECK (a > 0)",
        );
        let bytes = run_with(&mut e, &mut b, "INSERT INTO ch VALUES (3, -1, 30)");
        assert!(
            String::from_utf8_lossy(&bytes).contains("23514"),
            "expected check enforced"
        );
        // ADD UNIQUE, then DROP by the generated name lifts enforcement.
        run_with(&mut e, &mut b, "ALTER TABLE ch ADD UNIQUE (b)");
        let bytes = run_with(&mut e, &mut b, "INSERT INTO ch VALUES (4, 8, 10)");
        assert!(
            String::from_utf8_lossy(&bytes).contains("23505"),
            "expected unique enforced"
        );
        run_with(&mut e, &mut b, "ALTER TABLE ch DROP CONSTRAINT ch_b_key");
        run_with(&mut e, &mut b, "INSERT INTO ch VALUES (5, 9, 10)");
        // DROP of a missing constraint errors (42704).
        let bytes = run_with(&mut e, &mut b, "ALTER TABLE ch DROP CONSTRAINT nope");
        assert!(
            String::from_utf8_lossy(&bytes).contains("42704"),
            "expected undefined constraint"
        );
    }
    // The CHECK constraint survives a restart and stays enforced.
    let mut b = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut b).unwrap();
    let bytes = run_with(&mut e, &mut b, "INSERT INTO ch VALUES (6, -5, 60)");
    assert!(
        String::from_utf8_lossy(&bytes).contains("23514"),
        "check survives restart"
    );
    let bytes = run_with(&mut e, &mut b, "SELECT count(*) FROM ch");
    assert_eq!(data_rows(&bytes), ["3"]);
}

#[test]
fn alter_rename_constraint() {
    let config = test_config("rename-constraint");
    let mut b = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut b).unwrap();
    run_with(&mut e, &mut b, "CREATE TABLE rc (id int, a int, b int)");
    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE rc ADD CONSTRAINT ck0 CHECK (a > 0)",
    );
    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE rc ADD CONSTRAINT u UNIQUE (b, id)",
    );
    run_with(&mut e, &mut b, "ALTER TABLE rc RENAME CONSTRAINT ck0 TO ck");
    run_with(&mut e, &mut b, "ALTER TABLE rc RENAME CONSTRAINT u TO u2");
    // Onto an existing name is 42710; a missing old name is 42704.
    let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, "ALTER TABLE rc ADD CONSTRAINT keep CHECK (a < 9); ALTER TABLE rc RENAME CONSTRAINT ck TO keep")).to_string();
    assert!(text.contains("42710"), "{text}");
    let text = String::from_utf8_lossy(&run_with(
        &mut e,
        &mut b,
        "ALTER TABLE rc RENAME CONSTRAINT nope TO whatever",
    ))
    .to_string();
    assert!(text.contains("42704"), "{text}");
    // The renamed CHECK is still enforced and droppable by its new name.
    let text = String::from_utf8_lossy(&run_with(
        &mut e,
        &mut b,
        "INSERT INTO rc VALUES (1, -1, 1)",
    ))
    .to_string();
    assert!(text.contains("23514"), "{text}");
    run_with(&mut e, &mut b, "ALTER TABLE rc DROP CONSTRAINT ck");
    let text = String::from_utf8_lossy(&run_with(
        &mut e,
        &mut b,
        "INSERT INTO rc VALUES (2, -1, 2)",
    ))
    .to_string();
    assert!(!text.contains("ERROR"), "{text}");
}

#[test]
fn check_constraint_auto_naming() {
    let config = test_config("check-naming");
    let mut b = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut b).unwrap();
    // Four unnamed CHECKs: a>0 and a<100 and a<>50 each reference only `a`, so
    // they collide on cn_a_check and disambiguate to cn_a_check / cn_a_check1 /
    // cn_a_check2 in declaration order; a>b references two columns and is
    // cn_check. Each insert below violates exactly one, so the reported name is
    // unambiguous.
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE cn (a int CHECK (a > 0), b int, CHECK (a > b), CHECK (a < 100), CHECK (a <> 50))",
    );
    for (sql, name) in [
        ("INSERT INTO cn VALUES (-1, -9)", "cn_a_check\""),
        ("INSERT INTO cn VALUES (5, 10)", "cn_check\""),
        ("INSERT INTO cn VALUES (200, 0)", "cn_a_check1\""),
        ("INSERT INTO cn VALUES (50, 0)", "cn_a_check2\""),
    ] {
        let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, sql)).to_string();
        assert!(
            text.contains("23514") && text.contains(name),
            "{sql} => {text}"
        );
    }
    // A column-level CHECK naming only another column is keyed off that column.
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE cm (a int CHECK (b > 0), b int)",
    );
    let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO cm VALUES (1, -1)"))
        .to_string();
    assert!(
        text.contains("23514") && text.contains("cm_b_check\""),
        "{text}"
    );
    // An explicit name wins and is not disambiguated; the later unnamed CHECK on
    // the same column takes the base generated name.
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE ck (a int CONSTRAINT keep_me CHECK (a > 0), CHECK (a < 100))",
    );
    let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO ck VALUES (-1)"))
        .to_string();
    assert!(text.contains("keep_me\""), "{text}");
    let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO ck VALUES (200)"))
        .to_string();
    assert!(text.contains("ck_a_check\""), "{text}");
    // ALTER TABLE ADD CHECK auto-names identically and the generated name is
    // what DROP CONSTRAINT uses.
    run_with(&mut e, &mut b, "ALTER TABLE cm ADD CHECK (a < 10)");
    let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO cm VALUES (20, 1)"))
        .to_string();
    assert!(
        text.contains("23514") && text.contains("cm_a_check\""),
        "{text}"
    );
    run_with(&mut e, &mut b, "ALTER TABLE cm DROP CONSTRAINT cm_a_check");
    let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO cm VALUES (20, 1)"))
        .to_string();
    assert!(!text.contains("ERROR"), "{text}");
}

#[test]
fn value_index_matches_uniqueness_oracle() {
    // The value index must give exactly the verdicts a full scan would.
    // A deterministic workload of inserts/updates/deletes over a small key space
    // (frequent collisions) is checked against a ground-truth set of committed
    // keys, across a restart (which rebuilds the indexes from replay).
    let config = test_config("value-index-oracle");
    let mut rng: u64 = 0x243f_6a88_85a3_08d3;
    let mut next = move || {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        rng = rng.wrapping_mul(0x2545_f491_4f6c_dd1d);
        rng
    };
    let mut present: std::collections::HashSet<i64> = std::collections::HashSet::new();
    // Each statement gets a fresh scratch budget: the harness's per-statement
    // draws are not reclaimed, and this workload runs a thousand of them.
    let run = |e: &mut Engine, sql: &str| {
        String::from_utf8_lossy(&run_with(e, &mut Budget::new(1 << 20), sql)).to_string()
    };

    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        run(&mut e, "CREATE TABLE t (k int UNIQUE, v int)");
        for _ in 0..800 {
            let key = (next() % 50) as i64;
            match next() % 3 {
                0 => {
                    let out = run(
                        &mut e,
                        &format!("INSERT INTO t VALUES ({}, {})", key, next() % 1000),
                    );
                    if present.contains(&key) {
                        assert!(out.contains("23505"), "dup insert {key}: {out}");
                    } else {
                        assert!(!out.contains("ERROR"), "new insert {key}: {out}");
                        present.insert(key);
                    }
                }
                1 => {
                    run(&mut e, &format!("DELETE FROM t WHERE k = {key}"));
                    present.remove(&key);
                }
                _ => {
                    let to = (next() % 50) as i64;
                    let out = run(&mut e, &format!("UPDATE t SET k = {to} WHERE k = {key}"));
                    if present.contains(&key) {
                        if to != key && present.contains(&to) {
                            assert!(out.contains("23505"), "update {key}->{to} dup: {out}");
                        } else {
                            assert!(!out.contains("ERROR"), "update {key}->{to}: {out}");
                            present.remove(&key);
                            present.insert(to);
                        }
                    } else {
                        assert!(!out.contains("ERROR"), "update absent {key}: {out}");
                    }
                }
            }
        }
    }

    // Restart: the index is gone and must be rebuilt from the replayed rows.
    let mut b = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut b).unwrap();
    let bytes = run_with(&mut e, &mut Budget::new(1 << 20), "SELECT count(*) FROM t");
    assert_eq!(data_rows(&bytes), [format!("{}", present.len())]);
    for _ in 0..200 {
        let key = (next() % 50) as i64;
        let out = run(&mut e, &format!("INSERT INTO t VALUES ({key}, 1)"));
        if present.contains(&key) {
            assert!(out.contains("23505"), "post-restart dup {key}: {out}");
        } else {
            assert!(!out.contains("ERROR"), "post-restart new {key}: {out}");
            present.insert(key);
        }
    }
}

#[test]
fn named_single_column_key_retains_name() {
    let config = test_config("named-single-key");
    let mut b = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut b).unwrap();
    // An explicit name on a single-column UNIQUE is kept: the violation names it
    // and DROP CONSTRAINT finds it.
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE t (a int CONSTRAINT myc UNIQUE, b int)",
    );
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (1, 1)");
    let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO t VALUES (1, 2)"))
        .to_string();
    assert!(text.contains("23505") && text.contains("myc\""), "{text}");
    run_with(&mut e, &mut b, "ALTER TABLE t DROP CONSTRAINT myc");
    let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO t VALUES (1, 3)"))
        .to_string();
    assert!(!text.contains("ERROR"), "drop by name: {text}");
    // A named single-column PRIMARY KEY: the violation names it, and DROP NOT
    // NULL on its column is rejected (the key implies NOT NULL).
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE p (id int CONSTRAINT p_id PRIMARY KEY, v int)",
    );
    run_with(&mut e, &mut b, "INSERT INTO p VALUES (1, 1)");
    let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO p VALUES (1, 2)"))
        .to_string();
    assert!(text.contains("23505") && text.contains("p_id\""), "{text}");
    let text = String::from_utf8_lossy(&run_with(
        &mut e,
        &mut b,
        "ALTER TABLE p ALTER COLUMN id DROP NOT NULL",
    ))
    .to_string();
    assert!(
        text.contains("42P16") && text.contains("primary key"),
        "{text}"
    );
    // Renaming an unnamed single-column key by its synthesized name materializes
    // it as a named key; the new name then enforces and drops.
    run_with(&mut e, &mut b, "CREATE TABLE u (x int UNIQUE)");
    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE u RENAME CONSTRAINT u_x_key TO xkey",
    );
    run_with(&mut e, &mut b, "INSERT INTO u VALUES (5)");
    let text =
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO u VALUES (5)")).to_string();
    assert!(
        text.contains("23505") && text.contains("xkey\""),
        "rename materialize: {text}"
    );
    run_with(&mut e, &mut b, "ALTER TABLE u DROP CONSTRAINT xkey");
    let text =
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO u VALUES (5)")).to_string();
    assert!(!text.contains("ERROR"), "drop renamed: {text}");
}

#[test]
fn alter_table_multi_action() {
    let config = test_config("alter-multi");
    let mut b = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut b).unwrap();
    run_with(&mut e, &mut b, "CREATE TABLE m (a int)");
    run_with(&mut e, &mut b, "INSERT INTO m VALUES (1), (2), (3)");
    // Several ADD COLUMNs with defaults applied in one statement.
    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE m ADD COLUMN b int DEFAULT 10, ADD COLUMN c text DEFAULT 'x'",
    );
    let bytes = run_with(&mut e, &mut b, "SELECT a, b, c FROM m ORDER BY a");
    assert_eq!(data_rows(&bytes), ["1|10|x", "2|10|x", "3|10|x"]);
    // Pass ordering: an ADD CONSTRAINT can reference a column ADDed later in the
    // same statement even though it is written first.
    let text = String::from_utf8_lossy(&run_with(
        &mut e,
        &mut b,
        "ALTER TABLE m ADD CONSTRAINT dpos CHECK (d > 0), ADD COLUMN d int DEFAULT 1",
    ))
    .to_string();
    assert!(!text.contains("ERROR"), "{text}");
    // A type change composes with a SET NOT NULL validated on the new image.
    let text = String::from_utf8_lossy(&run_with(
        &mut e,
        &mut b,
        "ALTER TABLE m ALTER COLUMN a TYPE bigint, ALTER COLUMN a SET NOT NULL",
    ))
    .to_string();
    assert!(!text.contains("ERROR"), "{text}");
    // A uniqueness constraint added alongside a rewrite is validated across the
    // rewritten images: a constant-default column collides on every row (23505).
    let text = String::from_utf8_lossy(&run_with(
        &mut e,
        &mut b,
        "ALTER TABLE m ADD COLUMN u int DEFAULT 7, ADD UNIQUE (u)",
    ))
    .to_string();
    assert!(text.contains("23505"), "{text}");
    // That failed ALTER left the table untouched — `u` was never added.
    let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT u FROM m")).to_string();
    assert!(text.contains("42703"), "u should not exist: {text}");
    // A mid-list error is atomic: the ADD before the bad DROP does not apply.
    let text = String::from_utf8_lossy(&run_with(
        &mut e,
        &mut b,
        "ALTER TABLE m ADD COLUMN g int, DROP COLUMN nope",
    ))
    .to_string();
    assert!(text.contains("42703"), "{text}");
    let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT g FROM m")).to_string();
    assert!(text.contains("42703"), "g should not exist: {text}");
    // DROP one column and ADD another in one statement.
    run_with(
        &mut e,
        &mut b,
        "ALTER TABLE m DROP COLUMN c, ADD COLUMN h int DEFAULT 99",
    );
    let bytes = run_with(&mut e, &mut b, "SELECT a, b, d, h FROM m ORDER BY a");
    assert_eq!(data_rows(&bytes), ["1|10|1|99", "2|10|1|99", "3|10|1|99"]);
}

#[test]
fn vacuum_and_analyze() {
    let config = test_config("vacuum");
    let mut b = Budget::new(1 << 26);
    let mut e = Engine::new(&config, &mut b).unwrap();
    run_with(&mut e, &mut b, "CREATE TABLE vt (a int, b text)");
    run_with(&mut e, &mut b, "INSERT INTO vt VALUES (1, 'x'), (2, 'y')");
    // The various forms parse and succeed, returning the command tag.
    for cmd in [
        "VACUUM",
        "VACUUM vt",
        "VACUUM FULL vt",
        "VACUUM (FULL, ANALYZE) vt",
        "VACUUM ANALYZE vt, vt",
    ] {
        let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, cmd)).to_string();
        assert!(text.contains("VACUUM"), "{cmd}: {text}");
    }
    for cmd in ["ANALYZE", "ANALYZE vt", "ANALYZE vt (a, b)"] {
        let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, cmd)).to_string();
        assert!(text.contains("ANALYZE"), "{cmd}: {text}");
    }
    let missing_table = run_with(&mut e, &mut b, "ANALYZE missing_table");
    assert!(
        String::from_utf8_lossy(&missing_table).contains("42P01"),
        "{}",
        String::from_utf8_lossy(&missing_table)
    );
    let missing_column = run_with(&mut e, &mut b, "ANALYZE vt (missing_column)");
    assert!(
        String::from_utf8_lossy(&missing_column).contains("42703"),
        "{}",
        String::from_utf8_lossy(&missing_column)
    );
    // The data is untouched.
    let bytes = run_with(&mut e, &mut b, "SELECT count(*) FROM vt");
    assert_eq!(data_rows(&bytes), ["2"]);
    // VACUUM is non-transactional (25001); ANALYZE is allowed.
    let text = String::from_utf8_lossy(&run_with(&mut e, &mut b, "BEGIN; VACUUM vt; ROLLBACK"))
        .to_string();
    assert!(text.contains("25001"), "{text}");
    let text =
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "BEGIN; ANALYZE vt; COMMIT")).to_string();
    assert!(!text.contains("25001") && !text.contains("ERROR"), "{text}");

    let slot = e.storage.find_table("public", "vt").expect("table exists");
    let statistics = e.storage.table_statistics(slot, 0);
    assert!(statistics.valid);
    assert_eq!(statistics.rows, 2);
    assert_eq!(statistics.columns[0].distinct_values, 2);
    assert_eq!(statistics.columns[1].distinct_values, 2);
    assert_eq!(statistics.columns[0].null_fraction_ppm, 0);
    assert!(statistics.average_row_width > 0);
    let catalog = run_with(
        &mut e,
        &mut b,
        "SELECT reltuples FROM pg_class WHERE relname = 'vt'; \
         SELECT attname, null_frac, avg_width, n_distinct FROM pg_stats \
         WHERE tablename = 'vt' ORDER BY attname",
    );
    let rows = data_rows(&catalog);
    assert_eq!(rows[0], "2");
    assert!(rows[1].starts_with("a|0|"), "{rows:?}");
    assert!(rows[1].ends_with("|-1"), "{rows:?}");
    assert!(rows[2].starts_with("b|0|"), "{rows:?}");
    assert!(rows[2].ends_with("|-1"), "{rows:?}");

    // PostgreSQL deliberately splits ANALYZE's transaction behavior:
    // pg_class relation estimates are updated in place, while pg_statistic
    // column rows roll back. Savepoints obey the same split.
    let transaction_statistics = data_rows(&run_with(
        &mut e,
        &mut b,
        "BEGIN; \
         INSERT INTO vt VALUES (NULL, 'rolled back'); \
         ANALYZE vt; \
         SELECT c.reltuples, s.null_frac FROM pg_class c JOIN pg_stats s \
           ON s.tablename = c.relname AND s.attname = 'a' WHERE c.relname = 'vt'; \
         ROLLBACK; \
         SELECT c.reltuples, s.null_frac FROM pg_class c JOIN pg_stats s \
           ON s.tablename = c.relname AND s.attname = 'a' WHERE c.relname = 'vt'",
    ));
    assert_eq!(
        transaction_statistics.len(),
        2,
        "{transaction_statistics:?}"
    );
    assert!(
        transaction_statistics[0].starts_with("3|0.333333"),
        "{transaction_statistics:?}"
    );
    assert_eq!(transaction_statistics[1], "3|0");
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT count(*) FROM vt")),
        ["2"]
    );

    let mut statistics_owner = TxnState::new(&mut b, 256).unwrap();
    let mut statistics_observer = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut statistics_owner, "BEGIN");
    run_txn(
        &mut e,
        &mut b,
        &mut statistics_owner,
        "INSERT INTO vt VALUES (NULL, 'private'); ANALYZE vt",
    );
    assert!(
        data_rows(&run_with_txn_bytes(
            &mut e,
            &mut b,
            &mut statistics_owner,
            "SELECT null_frac FROM pg_stats WHERE tablename = 'vt' AND attname = 'a'"
        ))[0]
            .starts_with("0.333333")
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut e,
            &mut b,
            &mut statistics_observer,
            "SELECT c.reltuples, s.null_frac FROM pg_class c JOIN pg_stats s \
             ON s.tablename = c.relname AND s.attname = 'a' WHERE c.relname = 'vt'"
        )),
        ["3|0"],
        "relation estimates are global in-place state, column statistics remain private"
    );
    run_txn(&mut e, &mut b, &mut statistics_owner, "ROLLBACK");

    let savepoint_statistics = data_rows(&run_with(
        &mut e,
        &mut b,
        "BEGIN; \
         INSERT INTO vt VALUES (NULL, 'rolled back'); \
         SAVEPOINT before_analyze; \
         ANALYZE vt; \
         SELECT null_frac FROM pg_stats WHERE tablename = 'vt' AND attname = 'a'; \
         ROLLBACK TO before_analyze; \
         SELECT null_frac FROM pg_stats WHERE tablename = 'vt' AND attname = 'a'; \
         ROLLBACK",
    ));
    assert_eq!(savepoint_statistics.len(), 2, "{savepoint_statistics:?}");
    assert!(
        savepoint_statistics[0].starts_with("0.333333"),
        "{savepoint_statistics:?}"
    );
    assert_eq!(savepoint_statistics[1], "0");

    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE targeted_stats (a int, b text); \
         INSERT INTO targeted_stats VALUES (1, 'one'); \
         ANALYZE targeted_stats(a)",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT attname FROM pg_stats WHERE tablename = 'targeted_stats'"
        )),
        ["a"]
    );
    run_with(
        &mut e,
        &mut b,
        "TRUNCATE targeted_stats; ANALYZE targeted_stats(a)",
    );
    assert!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT attname FROM pg_stats WHERE tablename = 'targeted_stats'"
        ))
        .is_empty()
    );
}

#[test]
fn analyze_statistics_recover_from_wal_with_postgresql_rollback_semantics() {
    let config = test_config("analyze-wal-recovery");
    {
        let mut budget = Budget::new(1 << 26);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        run_with(
            &mut engine,
            &mut budget,
            "CREATE TABLE durable_statistics (a int); \
             INSERT INTO durable_statistics VALUES (1), (2); \
             ANALYZE durable_statistics",
        );
        run_with(
            &mut engine,
            &mut budget,
            "BEGIN; \
             INSERT INTO durable_statistics VALUES (NULL); \
             ANALYZE durable_statistics; \
             ROLLBACK",
        );
        // A following commit carries the rollback-surviving in-place relation
        // estimate into WAL without resurrecting the rolled-back column row.
        run_with(&mut engine, &mut budget, "SELECT 1");
        run_with(
            &mut engine,
            &mut budget,
            "CREATE TABLE targeted_stale_statistics (a int, b int); \
             INSERT INTO targeted_stale_statistics VALUES (1, 10), (2, 20); \
             ANALYZE targeted_stale_statistics; \
             DELETE FROM targeted_stale_statistics WHERE a = 2; \
             ANALYZE targeted_stale_statistics(a)",
        );
    }

    let mut budget = Budget::new(1 << 26);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT c.reltuples, s.null_frac, (SELECT count(*) FROM durable_statistics) \
             FROM pg_class c JOIN pg_stats s \
               ON s.tablename = c.relname AND s.attname = 'a' \
             WHERE c.relname = 'durable_statistics'"
        )),
        ["3|0|2"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT c.reltuples, s.n_distinct \
             FROM pg_class c JOIN pg_stats s \
               ON s.tablename = c.relname AND s.attname = 'b' \
             WHERE c.relname = 'targeted_stale_statistics'"
        )),
        ["1|-1"],
        "targeted ANALYZE preserves an untouched estimate even when it exceeds the new row estimate"
    );
    drop(engine);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
}

#[test]
fn explain_uses_statistics_and_analyze_executes_without_returning_query_rows() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE ep (id int PRIMARY KEY, payload text)",
    );
    run_with(
        &mut engine,
        &mut budget,
        "INSERT INTO ep VALUES (1, 'a'), (2, 'b'), (3, 'c'); ANALYZE ep",
    );

    let explained = run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN SELECT payload FROM ep WHERE id > 1 ORDER BY payload",
    );
    let rows = data_rows(&explained);
    assert!(
        rows.iter().any(|row| row.starts_with("Sort  (cost=")),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("Seq Scan on ep")),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.starts_with("Planning Time: ")),
        "{rows:?}"
    );

    let analyzed = run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN (ANALYZE, BUFFERS, TIMING OFF) SELECT payload FROM ep WHERE id = 2",
    );
    let rows = data_rows(&analyzed);
    assert!(
        rows.iter()
            .any(|row| row.contains("(actual rows=1.00 loops=1)")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.starts_with("  Buffers: shared hit=")),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.starts_with("Execution Time: ")),
        "{rows:?}"
    );

    let invalid = run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN (BUFFERS) SELECT * FROM ep",
    );
    assert!(String::from_utf8_lossy(&invalid).contains("22023"));
    let invalid_timing = run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN (TIMING ON) SELECT * FROM ep",
    );
    assert!(String::from_utf8_lossy(&invalid_timing).contains("22023"));
    let invalid_serialize = run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN (SERIALIZE TEXT) SELECT * FROM ep",
    );
    assert!(String::from_utf8_lossy(&invalid_serialize).contains("22023"));
    let invalid_generic = run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN (GENERIC_PLAN, ANALYZE) SELECT * FROM ep",
    );
    assert!(String::from_utf8_lossy(&invalid_generic).contains("22023"));

    let detailed = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN (ANALYZE, VERBOSE, MEMORY, SERIALIZE, TIMING OFF) \
         SELECT payload FROM ep WHERE id = 2",
    ));
    assert!(detailed.iter().any(|row| row.contains("Output: payload")));
    assert!(detailed.iter().any(|row| row == "Planning:"));
    assert!(
        detailed
            .iter()
            .any(|row| row.starts_with("Serialization: time=")),
        "{detailed:?}"
    );
    assert!(
        detailed
            .iter()
            .any(|row| row.contains("Index Scan using ep_pkey"))
    );

    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE ep_large (id int); CREATE TABLE ep_small (id int); \
         INSERT INTO ep_large SELECT g FROM generate_series(1, 20) g; \
         INSERT INTO ep_small VALUES (1); ANALYZE ep_large; ANALYZE ep_small",
    );
    let join_plan = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN SELECT count(*) FROM ep_large, ep_small",
    ));
    let small = join_plan
        .iter()
        .position(|row| row.contains("Seq Scan on ep_small"))
        .expect("small scan is planned");
    let large = join_plan
        .iter()
        .position(|row| row.contains("Seq Scan on ep_large"))
        .expect("large scan is planned");
    assert!(
        small < large,
        "EXPLAIN must expose the cardinality-based executor order: {join_plan:?}"
    );

    let set_plan = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN SELECT 1 UNION ALL SELECT 2",
    ));
    assert!(
        set_plan.iter().any(|row| row.contains("Append")),
        "{set_plan:?}"
    );

    let insert_plan = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN INSERT INTO ep VALUES (4, 'd')",
    ));
    assert!(
        insert_plan
            .iter()
            .any(|row| row.starts_with("Insert on ep")),
        "{insert_plan:?}"
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT count(*) FROM ep"
        )),
        ["3"]
    );
    let analyzed_insert = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN (ANALYZE, TIMING OFF) INSERT INTO ep VALUES (4, 'd')",
    ));
    assert!(
        analyzed_insert
            .iter()
            .any(|row| row.starts_with("Insert on ep") && row.contains("actual rows=0.00")),
        "{analyzed_insert:?}"
    );
    let returning_insert = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN (ANALYZE, TIMING OFF) INSERT INTO ep VALUES (5, 'e') RETURNING id",
    ));
    assert!(
        returning_insert
            .iter()
            .any(|row| row.starts_with("Insert on ep") && row.contains("actual rows=0.00")),
        "{returning_insert:?}"
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT count(*) FROM ep"
        )),
        ["5"]
    );
    let wal_update = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN (ANALYZE, WAL, TIMING OFF) \
         UPDATE ep SET payload = 'updated' WHERE id = 4",
    ));
    assert!(
        wal_update
            .iter()
            .any(|row| row.starts_with("  WAL: records=") && !row.contains("records=0")),
        "{wal_update:?}"
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT payload FROM ep WHERE id = 4"
        )),
        ["updated"]
    );

    let json = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN (FORMAT JSON) SELECT 1",
    ));
    assert_eq!(json.len(), 1);
    assert!(json[0].starts_with("[{\"Plan\":{"), "{json:?}");
    assert!(json[0].contains("\"Node Type\":\"Result\""), "{json:?}");

    let xml = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN (FORMAT XML, SUMMARY OFF) SELECT 1",
    ));
    assert!(xml[0].starts_with("<explain "), "{xml:?}");
    assert!(xml[0].contains("<Node-Type>Result</Node-Type>"), "{xml:?}");

    let yaml = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "EXPLAIN (FORMAT YAML) SELECT 1",
    ));
    assert!(yaml[0].starts_with("- Plan:\n"), "{yaml:?}");
    assert!(yaml[0].contains("Node Type: \"Result\""), "{yaml:?}");
}

#[test]
fn joins_group_by_subqueries() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE d (id int, name text)");
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE emp (id int, did int, name text, pay int)",
    );
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO d VALUES (1,'eng'),(2,'ops'),(3,'none')",
    );
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO emp VALUES (1,1,'ada',120),(2,1,'bob',100),(3,2,'cyd',90),(4,NULL,'dee',80)",
    );

    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT e.name, d.name FROM emp e JOIN d ON e.did = d.id ORDER BY e.id",
    );
    assert_eq!(data_rows(&bytes), ["ada|eng", "bob|eng", "cyd|ops"]);

    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT e.name, d.name FROM emp e LEFT JOIN d ON e.did = d.id ORDER BY e.id",
    );
    assert_eq!(
        data_rows(&bytes),
        ["ada|eng", "bob|eng", "cyd|ops", "dee|NULL"]
    );

    let bytes = run_with(&mut e, &mut b, "SELECT count(*) FROM emp, d");
    assert_eq!(data_rows(&bytes), ["12"]);

    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT d.name, count(*), sum(e.pay) FROM emp e JOIN d ON e.did = d.id \
         GROUP BY d.name HAVING count(*) > 1",
    );
    assert_eq!(data_rows(&bytes), ["eng|2|220"]);

    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT name FROM emp WHERE pay > (SELECT avg(pay) FROM emp) ORDER BY name",
    );
    assert_eq!(data_rows(&bytes), ["ada", "bob"]);

    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT name FROM d WHERE id IN (SELECT did FROM emp) ORDER BY name",
    );
    assert_eq!(data_rows(&bytes), ["eng", "ops"]);
    // NOT IN with a NULL member yields no rows (SQL three-valued logic).
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT name FROM d WHERE id NOT IN (SELECT did FROM emp) ORDER BY name",
    );
    assert_eq!(data_rows(&bytes), Vec::<String>::new());

    // UPDATE with an IN-subquery.
    run_with(
        &mut e,
        &mut b,
        "UPDATE emp SET pay = 0 WHERE did IN (SELECT id FROM d WHERE name = 'ops')",
    );
    let bytes = run_with(&mut e, &mut b, "SELECT pay FROM emp WHERE name = 'cyd'");
    assert_eq!(data_rows(&bytes), ["0"]);

    // Ambiguity and qualification errors.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT name FROM emp e JOIN d ON e.did = d.id",
    );
    assert!(String::from_utf8_lossy(&bytes).contains("42702"));
}

#[test]
fn datetime_uuid_bytea_types() {
    let config = test_config("types-durable");
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        run_with(
            &mut e,
            &mut b,
            "CREATE TABLE ev (d date, t timestamptz, u uuid, raw bytea)",
        );
        run_with(
            &mut e,
            &mut b,
            "INSERT INTO ev VALUES ('2024-02-29', '2024-02-29 12:00:00+02', \
             'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '\\xdeadbeef')",
        );
        let bytes = run_with(&mut e, &mut b, "SELECT d, t, u, raw FROM ev");
        assert_eq!(
            data_rows(&bytes),
            ["2024-02-29|2024-02-29 10:00:00+00|a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11|\\xdeadbeef"]
        );
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT count(*) FROM ev WHERE d = '2024-02-29' AND t < '2025-01-01'",
        );
        assert_eq!(data_rows(&bytes), ["1"]);
    }
    // Types survive WAL replay.
    let mut b = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut b).unwrap();
    let bytes = run_with(&mut e, &mut b, "SELECT u FROM ev");
    assert_eq!(data_rows(&bytes), ["a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"]);
    let bytes = run_with(&mut e, &mut b, "SELECT 'bad-uuid'::uuid");
    assert!(String::from_utf8_lossy(&bytes).contains("22P02"));
}

#[test]
fn comment_roundtrip_and_removal() {
    let (mut e, mut b) = test_engine();
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE ct (id int PRIMARY KEY, a text)",
    );
    run_with(
        &mut e,
        &mut b,
        "CREATE VIEW cv AS SELECT a AS renamed FROM ct",
    );
    run_with(&mut e, &mut b, "CREATE TYPE mood AS ENUM ('low', 'high')");
    run_with(
        &mut e,
        &mut b,
        "CREATE DOMAIN positive AS int CHECK (VALUE > 0)",
    );
    run_with(&mut e, &mut b, "CREATE SCHEMA cs");
    run_with(&mut e, &mut b, "CREATE TABLE cs.source (value int)");
    let mut guc = GucState::new();
    run_session(&mut e, &mut b, &mut guc, "SET search_path = cs, public");
    let bytes = run_session(
        &mut e,
        &mut b,
        &mut guc,
        "CREATE VIEW public.captured AS SELECT value AS captured_value FROM source",
    );
    assert_eq!(message_types(&bytes), [b'C']);
    run_session(&mut e, &mut b, &mut guc, "SET search_path = public");
    run_with(&mut e, &mut b, "COMMENT ON TABLE ct IS 'the table'");
    run_with(&mut e, &mut b, "COMMENT ON COLUMN ct.a IS 'col a'");
    run_with(&mut e, &mut b, "COMMENT ON VIEW cv IS 'view object'");
    run_with(&mut e, &mut b, "COMMENT ON COLUMN cv.renamed IS 'view col'");
    run_with(&mut e, &mut b, "COMMENT ON TYPE cv IS 'view row type'");
    run_with(&mut e, &mut b, "COMMENT ON TYPE mood IS 'the enum'");
    run_with(&mut e, &mut b, "COMMENT ON DOMAIN positive IS 'the domain'");
    run_with(&mut e, &mut b, "COMMENT ON TYPE integer IS 'the builtin'");
    run_with(&mut e, &mut b, "COMMENT ON TYPE regclass IS 'the regtype'");
    run_with(&mut e, &mut b, "COMMENT ON TYPE integer[] IS 'the array'");
    run_with(&mut e, &mut b, "COMMENT ON TYPE ct IS 'the row type'");
    let bytes = run_session(
        &mut e,
        &mut b,
        &mut guc,
        "COMMENT ON COLUMN captured.captured_value IS 'captured path col'",
    );
    assert_eq!(message_types(&bytes), [b'C']);
    run_with(&mut e, &mut b, "COMMENT ON SCHEMA cs IS 'the schema'");

    let bytes = run_with(&mut e, &mut b, "SELECT obj_description('ct'::regclass)");
    assert_eq!(data_rows(&bytes), ["the table"]);
    let bytes = run_with(&mut e, &mut b, "SELECT col_description('ct'::regclass, 2)");
    assert_eq!(data_rows(&bytes), ["col a"]);
    let bytes = run_with(&mut e, &mut b, "SELECT col_description('cv'::regclass, 1)");
    assert_eq!(data_rows(&bytes), ["view col"]);
    run_with(
        &mut e,
        &mut b,
        "CREATE OR REPLACE VIEW cv AS SELECT a AS renamed FROM ct WHERE id >= 0",
    );
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT obj_description('cv'::regclass), col_description('cv'::regclass, 1)",
    );
    assert_eq!(data_rows(&bytes), ["view object|view col"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT obj_description(oid, 'pg_type') FROM pg_type WHERE typname = 'cv'",
    );
    assert_eq!(data_rows(&bytes), ["view row type"]);
    let bytes = run_session(
        &mut e,
        &mut b,
        &mut guc,
        "SELECT col_description('captured'::regclass, 1)",
    );
    assert_eq!(
        data_rows(&bytes),
        ["captured path col"],
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT obj_description(oid, 'pg_type') FROM pg_type \
         WHERE typname IN ('int4', 'mood', 'positive') ORDER BY typname",
    );
    assert_eq!(data_rows(&bytes), ["the builtin", "the enum", "the domain"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT obj_description(2205, 'pg_type'), obj_description(1007, 'pg_type')",
    );
    assert_eq!(data_rows(&bytes), ["the regtype|the array"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT obj_description(oid, 'pg_type') FROM pg_type WHERE typname = 'ct'",
    );
    assert_eq!(data_rows(&bytes), ["the row type"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT description FROM pg_description \
         WHERE description LIKE 'the %' OR description IN ('col a', 'view col') \
         ORDER BY description",
    );
    assert_eq!(
        data_rows(&bytes),
        [
            "col a",
            "the array",
            "the builtin",
            "the domain",
            "the enum",
            "the regtype",
            "the row type",
            "the schema",
            "the table",
            "view col",
        ]
    );

    // Overwrite is last-write-wins; IS NULL removes.
    run_with(&mut e, &mut b, "COMMENT ON TABLE ct IS 'renamed'");
    let bytes = run_with(&mut e, &mut b, "SELECT obj_description('ct'::regclass)");
    assert_eq!(data_rows(&bytes), ["renamed"]);
    run_with(&mut e, &mut b, "COMMENT ON TABLE ct IS NULL");
    let bytes = run_with(&mut e, &mut b, "SELECT obj_description('ct'::regclass)");
    assert_eq!(data_rows(&bytes), ["NULL"]);
}

#[test]
fn comment_errors_match_postgres() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE ct (a int)");
    run_with(&mut e, &mut b, "CREATE VIEW cv AS SELECT a FROM ct");
    run_with(&mut e, &mut b, "CREATE INDEX ci ON ct (a)");
    run_with(&mut e, &mut b, "CREATE SEQUENCE cs");
    run_with(&mut e, &mut b, "CREATE TYPE mood AS ENUM ('low', 'high')");
    // Missing relation, wrong kind, missing column, missing schema.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "COMMENT ON TABLE nope IS 'x'"))
            .contains("42P01")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "COMMENT ON TABLE cv IS 'x'"))
            .contains("42809")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "COMMENT ON VIEW ct IS 'x'"))
            .contains("42809")
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "COMMENT ON COLUMN ct.nope IS 'x'"
        ))
        .contains("42703")
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "COMMENT ON COLUMN cv.nope IS 'x'"
        ))
        .contains("42703")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "COMMENT ON COLUMN ci.a IS 'x'"))
            .contains("42809")
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "COMMENT ON COLUMN cs.last_value IS 'x'"
        ))
        .contains("42809")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "COMMENT ON SCHEMA nope IS 'x'"))
            .contains("3F000")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "COMMENT ON TYPE nope IS 'x'"))
            .contains("42704")
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "COMMENT ON TYPE pg_catalog.integer IS 'x'"
        ))
        .contains("42704")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "COMMENT ON TYPE serial IS 'x'"))
            .contains("42704")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "COMMENT ON DOMAIN mood IS 'x'"))
            .contains("42809")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "COMMENT ON DOMAIN ct IS 'x'"))
            .contains("42809")
    );
}

#[test]
fn comment_rolls_back() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    run_txn(&mut e, &mut b, &mut t, "CREATE TABLE ct (a int)");
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "CREATE TYPE mood AS ENUM ('low', 'high')",
    );
    run_txn(&mut e, &mut b, &mut t, "COMMENT ON TABLE ct IS 'committed'");
    // A rolled-back overwrite restores the committed comment.
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    run_txn(&mut e, &mut b, &mut t, "COMMENT ON TABLE ct IS 'doomed'");
    run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
    let bytes = run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT obj_description('ct'::regclass)",
    );
    assert_eq!(data_rows(&bytes), ["committed"]);
    // A rolled-back fresh comment leaves none.
    run_txn(&mut e, &mut b, &mut t, "COMMENT ON TABLE ct IS NULL");
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    run_txn(&mut e, &mut b, &mut t, "COMMENT ON TABLE ct IS 'doomed'");
    run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
    let bytes = run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT obj_description('ct'::regclass)",
    );
    assert_eq!(data_rows(&bytes), ["NULL"]);

    // Catalog scans and helper functions both see the transaction's own
    // comment overlay, then return to the committed value after rollback.
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "COMMENT ON TYPE mood IS 'committed type'",
    );
    run_txn(&mut e, &mut b, &mut t, "BEGIN");
    run_txn(
        &mut e,
        &mut b,
        &mut t,
        "COMMENT ON TYPE mood IS 'doomed type'",
    );
    let bytes = run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT description FROM pg_description d JOIN pg_type t ON t.oid = d.objoid \
         WHERE t.typname = 'mood'",
    );
    assert_eq!(data_rows(&bytes), ["doomed type"]);
    run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
    let bytes = run_with_txn_bytes(
        &mut e,
        &mut b,
        &mut t,
        "SELECT obj_description(oid, 'pg_type') FROM pg_type WHERE typname = 'mood'",
    );
    assert_eq!(data_rows(&bytes), ["committed type"]);
}

#[test]
fn pg_restore_clean_owner_and_schema_cascade_surface() {
    let (mut e, mut b) = test_engine();
    let created = run_with(
        &mut e,
        &mut b,
        "CREATE SCHEMA restore;
         CREATE TYPE restore.mood AS ENUM ('ok');
         CREATE DOMAIN restore.positive AS integer CHECK (VALUE > 0);
         CREATE TABLE restore.items (
           id integer GENERATED BY DEFAULT AS IDENTITY,
           mood restore.mood,
           amount restore.positive,
           CONSTRAINT items_amount_key UNIQUE (amount)
         );
         CREATE SEQUENCE restore.ticket_seq;
         CREATE VIEW restore.item_view AS SELECT id,mood FROM restore.items;
         CREATE MATERIALIZED VIEW restore.item_counts AS
           SELECT mood,count(*) AS n FROM restore.items GROUP BY mood WITH NO DATA;",
    );
    assert!(
        !message_types(&created).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&created)
    );
    let comments = run_with(
        &mut e,
        &mut b,
        "COMMENT ON SCHEMA restore IS 'temporary';
         COMMENT ON TABLE restore.items IS 'temporary';",
    );
    assert_eq!(message_types(&comments), [b'C', b'C']);

    let owners = run_with(
        &mut e,
        &mut b,
        "ALTER SCHEMA restore OWNER TO postgres;
         ALTER TYPE restore.mood OWNER TO current_user;
         ALTER DOMAIN restore.positive OWNER TO session_user;
         ALTER TABLE restore.items OWNER TO postgres;
         ALTER VIEW restore.item_view OWNER TO postgres;
         ALTER MATERIALIZED VIEW restore.item_counts OWNER TO postgres;
         ALTER SEQUENCE restore.ticket_seq OWNER TO current_role;",
    );
    assert_eq!(
        message_types(&owners),
        [b'C', b'C', b'C', b'C', b'C', b'C', b'C']
    );
    assert_eq!(
        message_types(&run_with(
            &mut e,
            &mut b,
            "ALTER TABLE restore.item_view OWNER TO postgres;
             ALTER TABLE restore.item_counts OWNER TO postgres;
             ALTER TABLE restore.ticket_seq OWNER TO postgres;",
        )),
        [b'C', b'C', b'C']
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "ALTER TABLE restore.items OWNER TO absent_role"
        ))
        .contains("42704")
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "ALTER MATERIALIZED VIEW restore.items OWNER TO postgres"
        ))
        .contains("42809")
    );

    let missing = run_with(
        &mut e,
        &mut b,
        "ALTER TABLE IF EXISTS ONLY restore.missing
           DROP CONSTRAINT IF EXISTS missing_constraint",
    );
    assert!(String::from_utf8_lossy(&missing).contains("ALTER TABLE"));
    let existing = run_with(
        &mut e,
        &mut b,
        "ALTER TABLE IF EXISTS ONLY restore.items
           DROP CONSTRAINT IF EXISTS missing_constraint",
    );
    assert!(String::from_utf8_lossy(&existing).contains("ALTER TABLE"));

    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "DROP SCHEMA restore")).contains("2BP01")
    );
    run_with(
        &mut e,
        &mut b,
        "BEGIN; DROP SCHEMA restore CASCADE; ROLLBACK",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT count(*) FROM restore.item_view;
             SELECT nextval('restore.ticket_seq');
             SELECT 'ok'::restore.mood;
             SELECT 1::restore.positive"
        )),
        ["0", "1", "ok", "1"]
    );

    run_with(&mut e, &mut b, "DROP SCHEMA restore CASCADE");
    let recreated = run_with(
        &mut e,
        &mut b,
        "CREATE SCHEMA restore;
         CREATE TYPE restore.mood AS ENUM ('new');
         CREATE DOMAIN restore.positive AS integer;
         CREATE TABLE restore.items (id integer);
         CREATE SEQUENCE restore.ticket_seq;
         CREATE VIEW restore.item_view AS SELECT id FROM restore.items;
         CREATE MATERIALIZED VIEW restore.item_counts AS
           SELECT count(*) AS n FROM restore.items WITH NO DATA;",
    );
    assert!(
        !message_types(&recreated).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&recreated)
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT obj_description(oid,'pg_namespace')
               FROM pg_namespace WHERE nspname='restore'"
        )),
        ["NULL"],
        "schema comments must not leak into the recreated object"
    );
}

#[test]
fn type_cascade_never_leaves_cross_schema_columns_dangling() {
    let (mut e, mut b) = test_engine();
    run_with(
        &mut e,
        &mut b,
        "CREATE SCHEMA types;
         CREATE TYPE types.mood AS ENUM ('ok');
         CREATE DOMAIN types.positive AS integer CHECK (VALUE > 0);
         CREATE TABLE public.consumer (
           mood types.mood,
           amount types.positive
         );
         CREATE TABLE public.wide_consumer (
           mood_1 types.mood, mood_2 types.mood, mood_3 types.mood,
           mood_4 types.mood, mood_5 types.mood, mood_6 types.mood,
           mood_7 types.mood, mood_8 types.mood, mood_9 types.mood,
           keep integer
         );
         INSERT INTO public.consumer VALUES ('ok', 1);
         INSERT INTO public.wide_consumer
           VALUES ('ok','ok','ok','ok','ok','ok','ok','ok','ok',42);",
    );
    let dropped_type = run_with(&mut e, &mut b, "DROP TYPE types.mood CASCADE");
    assert!(
        !message_types(&dropped_type).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&dropped_type)
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT amount FROM public.consumer"
        )),
        ["1"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT keep FROM public.wide_consumer"
        )),
        ["42"],
        "all dependent columns in one table must be removed by one ALTER version"
    );
    let dropped_schema = run_with(&mut e, &mut b, "DROP SCHEMA types CASCADE");
    assert!(
        !message_types(&dropped_schema).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&dropped_schema)
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT count(*) FROM public.consumer"
        )),
        ["1"]
    );
}

#[test]
fn drop_schema_cascade_versions_surviving_foreign_keys() {
    let (mut engine, mut budget) = test_engine();
    let mut owner = TxnState::new(&mut budget, 256).unwrap();
    let mut observer = TxnState::new(&mut budget, 256).unwrap();
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "CREATE SCHEMA doomed;
         CREATE TABLE doomed.parent (id int PRIMARY KEY);
         CREATE TABLE child (parent_id int REFERENCES doomed.parent(id))",
    );
    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    let dropped = run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "DROP SCHEMA doomed CASCADE",
    );
    assert!(dropped.contains("DROP SCHEMA"), "{dropped}");
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut owner,
            "SELECT count(*) FROM pg_constraint c JOIN pg_class r ON r.oid = c.conrelid \
             WHERE r.relname = 'child' AND c.contype = 'f'",
        )),
        ["0"]
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut observer,
            "SELECT count(*) FROM pg_constraint c JOIN pg_class r ON r.oid = c.conrelid \
             WHERE r.relname = 'child' AND c.contype = 'f'",
        )),
        ["1"],
        "another transaction must retain the committed inbound constraint"
    );
    run_txn(&mut engine, &mut budget, &mut owner, "ROLLBACK");
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut observer,
            "SELECT count(*) FROM pg_constraint c JOIN pg_class r ON r.oid = c.conrelid \
             WHERE r.relname = 'child' AND c.contype = 'f'",
        )),
        ["1"]
    );
}

#[test]
fn drop_domain_cascade_removes_the_bounded_descendant_closure() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE SCHEMA domain_tree;
         CREATE DOMAIN domain_tree.root AS integer;
         CREATE DOMAIN domain_tree.branch AS domain_tree.root;
         CREATE DOMAIN domain_tree.leaf AS domain_tree.branch;",
    );
    let restricted = run_with(&mut engine, &mut budget, "DROP DOMAIN domain_tree.root");
    assert!(String::from_utf8_lossy(&restricted).contains("2BP01"));
    let dropped = run_with(
        &mut engine,
        &mut budget,
        "BEGIN;
         DROP DOMAIN domain_tree.root CASCADE;
         ROLLBACK;",
    );
    assert!(!message_types(&dropped).contains(&b'E'));
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT typname FROM pg_type
             WHERE typname IN ('root','branch','leaf')
             ORDER BY typname"
        )),
        ["branch", "leaf", "root"]
    );
    run_with(
        &mut engine,
        &mut budget,
        "DROP DOMAIN domain_tree.root CASCADE",
    );
    assert!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT typname FROM pg_type
         WHERE typname IN ('root','branch','leaf')"
        ))
        .is_empty()
    );
    let recreated = run_with(
        &mut engine,
        &mut budget,
        "CREATE DOMAIN domain_tree.root AS integer;
         CREATE DOMAIN domain_tree.branch AS domain_tree.root;
         CREATE DOMAIN domain_tree.leaf AS domain_tree.branch;",
    );
    assert!(!message_types(&recreated).contains(&b'E'));
}

#[test]
fn drop_enum_cascade_removes_dependent_domains() {
    let (mut engine, mut budget) = test_engine();
    let created = run_with(
        &mut engine,
        &mut budget,
        "CREATE SCHEMA enum_tree;
         CREATE TYPE enum_tree.root AS ENUM ('one');
         CREATE DOMAIN enum_tree.branch AS enum_tree.root;
         CREATE DOMAIN enum_tree.leaf AS enum_tree.branch;",
    );
    assert!(
        !message_types(&created).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&created)
    );
    let restricted = run_with(&mut engine, &mut budget, "DROP TYPE enum_tree.root");
    assert!(
        String::from_utf8_lossy(&restricted).contains("2BP01"),
        "{}",
        String::from_utf8_lossy(&restricted)
    );
    let rolled_back = run_with(
        &mut engine,
        &mut budget,
        "BEGIN; DROP TYPE enum_tree.root CASCADE; ROLLBACK",
    );
    assert!(!message_types(&rolled_back).contains(&b'E'));
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT typname FROM pg_type
             WHERE typname IN ('root','branch','leaf')
             ORDER BY typname"
        )),
        ["branch", "leaf", "root"]
    );
    let dropped = run_with(&mut engine, &mut budget, "DROP TYPE enum_tree.root CASCADE");
    assert!(!message_types(&dropped).contains(&b'E'));
    assert!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT typname FROM pg_type WHERE typname IN ('root','branch','leaf')"
        ))
        .is_empty()
    );
}

#[test]
fn drop_schema_cascade_handles_cross_schema_type_dependents_without_corruption() {
    let (mut engine, mut budget) = test_engine();
    let created = run_with(
        &mut engine,
        &mut budget,
        "CREATE SCHEMA type_roots;
         CREATE SCHEMA type_leaves;
         CREATE TYPE type_roots.mood AS ENUM ('one');
         CREATE DOMAIN type_roots.positive AS integer;
         CREATE DOMAIN type_leaves.mood_domain AS type_roots.mood;
         CREATE DOMAIN type_leaves.positive_domain AS type_roots.positive;",
    );
    assert!(
        !message_types(&created).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&created)
    );
    let rolled_back = run_with(
        &mut engine,
        &mut budget,
        "BEGIN; DROP SCHEMA type_roots CASCADE; ROLLBACK",
    );
    assert!(!message_types(&rolled_back).contains(&b'E'));
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT typname FROM pg_type
             WHERE typname IN ('mood','positive','mood_domain','positive_domain')
             ORDER BY typname"
        )),
        ["mood", "mood_domain", "positive", "positive_domain"]
    );
    let dropped = run_with(&mut engine, &mut budget, "DROP SCHEMA type_roots CASCADE");
    assert!(!message_types(&dropped).contains(&b'E'));
    assert!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT typname FROM pg_type
         WHERE typname IN ('mood','positive','mood_domain','positive_domain')"
        ))
        .is_empty()
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT nspname FROM pg_namespace WHERE nspname='type_leaves'"
        )),
        ["type_leaves"]
    );
}

#[test]
fn drop_schema_cascade_drops_external_stored_query_dependents() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE SCHEMA view_source;
         CREATE SCHEMA view_consumer;
         CREATE TABLE view_source.items (id integer);
         CREATE VIEW view_consumer.items AS SELECT id FROM view_source.items;",
    );
    let dropped = run_with(&mut engine, &mut budget, "DROP SCHEMA view_source CASCADE");
    assert!(!message_types(&dropped).contains(&b'E'));
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut engine,
            &mut budget,
            "SELECT count(*) FROM view_consumer.items"
        ))
        .contains("42P01")
    );
}

#[test]
fn type_drop_cascades_to_stored_query_dependents() {
    let (mut engine, mut budget) = test_engine();
    let created = run_with(
        &mut engine,
        &mut budget,
        "CREATE SCHEMA view_types;
         CREATE TYPE view_types.mood AS ENUM ('one');
         CREATE VIEW public.mood_view AS SELECT 'one'::view_types.mood AS mood;",
    );
    assert!(
        !message_types(&created).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&created)
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut engine,
            &mut budget,
            "DROP TYPE view_types.mood"
        ))
        .contains("2BP01")
    );
    let dropped = run_with(
        &mut engine,
        &mut budget,
        "DROP TYPE view_types.mood CASCADE",
    );
    assert!(!message_types(&dropped).contains(&b'E'));
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut engine,
            &mut budget,
            "SELECT mood FROM public.mood_view"
        ))
        .contains("42P01")
    );
}

#[test]
fn relation_drop_cascades_through_stored_query_dependency_closure() {
    let (mut engine, mut budget) = test_engine();
    let created = run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE dependency_root (id integer);
         CREATE VIEW dependency_view AS SELECT id FROM dependency_root;
         CREATE MATERIALIZED VIEW dependency_matview AS
             SELECT id FROM dependency_view WITH NO DATA;
         CREATE VIEW dependency_leaf AS SELECT id FROM dependency_matview;",
    );
    assert!(
        !message_types(&created).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&created)
    );
    let restricted = run_with(&mut engine, &mut budget, "DROP TABLE dependency_root");
    let restricted_text = String::from_utf8_lossy(&restricted);
    assert!(restricted_text.contains("2BP01"));
    assert!(
        restricted_text
            .contains("materialized view dependency_matview depends on view dependency_view")
    );
    let dropped = run_with(
        &mut engine,
        &mut budget,
        "DROP TABLE dependency_root CASCADE",
    );
    assert!(
        !message_types(&dropped).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&dropped)
    );
    assert!(
        String::from_utf8_lossy(&dropped)
            .contains("drop cascades to materialized view dependency_matview")
    );
    let catalogs = run_with(
        &mut engine,
        &mut budget,
        "SELECT
             (SELECT count(*) FROM pg_views WHERE viewname LIKE 'dependency_%'),
             (SELECT count(*) FROM pg_matviews WHERE matviewname LIKE 'dependency_%')",
    );
    assert_eq!(
        data_rows(&catalogs),
        ["0|0"],
        "{}",
        String::from_utf8_lossy(&catalogs)
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut engine,
            &mut budget,
            "SELECT * FROM dependency_view"
        ))
        .contains("42P01")
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut engine,
            &mut budget,
            "SELECT * FROM dependency_matview"
        ))
        .contains("42P01")
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut engine,
            &mut budget,
            "SELECT * FROM dependency_leaf"
        ))
        .contains("42P01")
    );
}

#[test]
fn drop_table_restricts_or_cascades_inbound_foreign_keys() {
    let (mut engine, mut budget) = test_engine();
    let created = run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE drop_parent (id integer PRIMARY KEY);
         CREATE TABLE drop_child (
             id integer PRIMARY KEY,
             parent_id integer REFERENCES drop_parent(id)
         );
         INSERT INTO drop_parent VALUES (1);
         INSERT INTO drop_child VALUES (1, 1);",
    );
    assert!(
        !message_types(&created).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&created)
    );

    let restricted = run_with(&mut engine, &mut budget, "DROP TABLE drop_parent");
    assert!(
        String::from_utf8_lossy(&restricted).contains("2BP01"),
        "{}",
        String::from_utf8_lossy(&restricted)
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT count(*) FROM drop_child"
        )),
        ["1"]
    );

    let cascaded = run_with(
        &mut engine,
        &mut budget,
        "DROP TABLE drop_parent CASCADE;
         INSERT INTO drop_child VALUES (2, 999);
         SELECT count(*) FROM drop_child;",
    );
    assert!(
        !message_types(&cascaded).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&cascaded)
    );
    assert_eq!(data_rows(&cascaded), ["2"]);
}

#[test]
fn sequence_drop_cascades_to_stored_query_dependents() {
    let (mut engine, mut budget) = test_engine();
    let created = run_with(
        &mut engine,
        &mut budget,
        "CREATE SEQUENCE dependency_sequence;
         CREATE VIEW dependency_sequence_view AS
             SELECT nextval('dependency_sequence') AS id;",
    );
    assert!(
        !message_types(&created).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&created)
    );
    let restricted = run_with(
        &mut engine,
        &mut budget,
        "DROP SEQUENCE dependency_sequence",
    );
    let restricted_text = String::from_utf8_lossy(&restricted);
    assert!(restricted_text.contains("2BP01"));
    assert!(
        restricted_text
            .contains("view dependency_sequence_view depends on sequence dependency_sequence")
    );
    let dropped = run_with(
        &mut engine,
        &mut budget,
        "DROP SEQUENCE dependency_sequence CASCADE",
    );
    assert!(!message_types(&dropped).contains(&b'E'));
    assert!(
        String::from_utf8_lossy(&dropped)
            .contains("drop cascades to view dependency_sequence_view")
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut engine,
            &mut budget,
            "SELECT * FROM dependency_sequence_view"
        ))
        .contains("42P01")
    );
}

#[test]
fn stored_query_binding_survives_relation_and_type_renames() {
    let (mut engine, mut budget) = test_engine();
    let created = run_with(
        &mut engine,
        &mut budget,
        "CREATE TYPE dependency_mood AS ENUM ('one');
         CREATE SCHEMA dependency_moved;
         CREATE TABLE dependency_named (id integer, mood dependency_mood);
         INSERT INTO dependency_named VALUES (7, 'one');
         CREATE VIEW dependency_named_view AS
             SELECT id, mood::public.dependency_mood AS mood
             FROM public.dependency_named;",
    );
    assert!(
        !message_types(&created).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&created)
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut engine,
            &mut budget,
            "DROP TABLE public.dependency_named"
        ))
        .contains("2BP01")
    );
    let renamed = run_with(
        &mut engine,
        &mut budget,
        "ALTER TABLE public.dependency_named RENAME TO dependency_renamed;
         ALTER TABLE public.dependency_renamed SET SCHEMA dependency_moved;
         ALTER TYPE public.dependency_mood RENAME TO dependency_feeling;",
    );
    assert!(
        !message_types(&renamed).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&renamed)
    );
    let selected = run_with(
        &mut engine,
        &mut budget,
        "SELECT id, mood::text FROM dependency_named_view",
    );
    assert_eq!(
        data_rows(&selected),
        ["7|one"],
        "{}",
        String::from_utf8_lossy(&selected)
    );
}

#[test]
fn stored_query_dependencies_survive_wal_replay() {
    let config = test_config("stored_query_dependencies_restart");
    {
        let mut budget = Budget::new(1 << 26);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let created = run_with(
            &mut engine,
            &mut budget,
            "CREATE TYPE durable_mood AS ENUM ('one');
             CREATE TABLE durable_source (id integer);
             INSERT INTO durable_source VALUES (11);
             CREATE VIEW durable_view AS
                 SELECT id, 'one'::durable_mood AS mood FROM durable_source;
             ALTER TABLE durable_source RENAME TO durable_renamed;
             ALTER TYPE durable_mood RENAME TO durable_feeling;",
        );
        assert!(
            !message_types(&created).contains(&b'E'),
            "{}",
            String::from_utf8_lossy(&created)
        );
    }
    let mut budget = Budget::new(1 << 26);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let selected = run_with(
        &mut engine,
        &mut budget,
        "SELECT id, mood::text FROM durable_view",
    );
    assert_eq!(
        data_rows(&selected),
        ["11|one"],
        "{}",
        String::from_utf8_lossy(&selected)
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut engine,
            &mut budget,
            "DROP TABLE durable_renamed"
        ))
        .contains("2BP01")
    );
    let dropped = run_with(
        &mut engine,
        &mut budget,
        "DROP TYPE durable_feeling CASCADE",
    );
    assert!(!message_types(&dropped).contains(&b'E'));
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut engine,
            &mut budget,
            "SELECT * FROM durable_view"
        ))
        .contains("42P01")
    );
}

#[test]
fn materialized_view_refresh_uses_captured_dependencies_after_rename() {
    let (mut engine, mut budget) = test_engine();
    let created = run_with(
        &mut engine,
        &mut budget,
        "CREATE TYPE refresh_mood AS ENUM ('one');
         CREATE TABLE refresh_source (id integer);
         INSERT INTO refresh_source VALUES (3);
         CREATE MATERIALIZED VIEW refresh_view AS
             SELECT id, 'one'::refresh_mood AS mood FROM refresh_source;
         ALTER TABLE refresh_source RENAME TO refresh_renamed;
         ALTER TYPE refresh_mood RENAME TO refresh_feeling;
         INSERT INTO refresh_renamed VALUES (4);
         REFRESH MATERIALIZED VIEW refresh_view;",
    );
    assert!(
        !message_types(&created).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&created)
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT id, mood::text FROM refresh_view ORDER BY id"
        )),
        ["3|one", "4|one"]
    );
}

#[test]
fn comment_survives_restart_and_drop_clears_it() {
    let config = test_config("comment-durable");
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        run_with(&mut e, &mut b, "CREATE TABLE ct (id int, a text)");
        run_with(&mut e, &mut b, "CREATE TYPE mood AS ENUM ('low', 'high')");
        run_with(&mut e, &mut b, "COMMENT ON TABLE ct IS 'durable'");
        run_with(&mut e, &mut b, "COMMENT ON COLUMN ct.a IS 'durable col'");
        run_with(&mut e, &mut b, "COMMENT ON TYPE mood IS 'durable type'");
    }
    // The comment survives WAL replay.
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        let bytes = run_with(&mut e, &mut b, "SELECT obj_description('ct'::regclass)");
        assert_eq!(data_rows(&bytes), ["durable"]);
        let bytes = run_with(&mut e, &mut b, "SELECT col_description('ct'::regclass, 2)");
        assert_eq!(data_rows(&bytes), ["durable col"]);
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT obj_description(oid, 'pg_type') FROM pg_type WHERE typname = 'mood'",
        );
        assert_eq!(data_rows(&bytes), ["durable type"]);
        // Dropping the table clears its comments; the freed name carries none.
        run_with(&mut e, &mut b, "DROP TABLE ct");
        run_with(&mut e, &mut b, "CREATE TABLE ct (id int, a text)");
        let bytes = run_with(&mut e, &mut b, "SELECT obj_description('ct'::regclass)");
        assert_eq!(data_rows(&bytes), ["NULL"]);
        // Type drops have the same cleanup guarantee.
        run_with(&mut e, &mut b, "DROP TYPE mood");
        run_with(&mut e, &mut b, "CREATE TYPE mood AS ENUM ('new')");
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT obj_description(oid, 'pg_type') FROM pg_type WHERE typname = 'mood'",
        );
        assert_eq!(data_rows(&bytes), ["NULL"]);
    }
    // The drop's comment removal is itself durable across another restart.
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        let bytes = run_with(&mut e, &mut b, "SELECT obj_description('ct'::regclass)");
        assert_eq!(data_rows(&bytes), ["NULL"]);
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT obj_description(oid, 'pg_type') FROM pg_type WHERE typname = 'mood'",
        );
        assert_eq!(data_rows(&bytes), ["NULL"]);
    }
}

#[test]
fn network_types_roundtrip_and_order() {
    let (mut e, mut b) = test_engine();
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE net (a inet, c cidr, m macaddr, m8 macaddr8)",
    );
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO net VALUES ('10.0.0.1/8','192.168.0.0/24','08:00:2b:01:02:03','08:00:2b:01:02:03:04:05'), \
         ('2001:db8::1', '10.0.0.0/8', '08-00-2b-01-02-04', '08:00:2b:01:02:03')",
    );
    let bytes = run_with(&mut e, &mut b, "SELECT a, c, m, m8 FROM net ORDER BY a");
    assert_eq!(
        data_rows(&bytes),
        [
            "10.0.0.1/8|192.168.0.0/24|08:00:2b:01:02:03|08:00:2b:01:02:03:04:05",
            "2001:db8::1|10.0.0.0/8|08:00:2b:01:02:04|08:00:2b:ff:fe:01:02:03",
        ]
    );
    // Casts and pg_typeof.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT '192.168.1.5/24'::inet::cidr, pg_typeof('10.0.0.1'::inet)",
    );
    assert_eq!(data_rows(&bytes), ["192.168.1.0/24|inet"]);
    // A bad literal errors 22P02.
    let bytes = run_with(&mut e, &mut b, "SELECT '999.1.1.1'::inet");
    assert!(
        String::from_utf8_lossy(&bytes).contains("22P02"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
}

#[test]
fn network_functions_match_postgres() {
    let (mut e, mut b) = test_engine();
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT family('192.168.1.5/24'::inet), host('192.168.1.5/24'::inet), masklen('192.168.1.5/24'::inet), \
                broadcast('192.168.1.5/24'::inet), netmask('192.168.1.5/24'::inet), hostmask('192.168.1.5/24'::inet), \
                network('192.168.1.5/24'::inet), abbrev('10.1.0.0/16'::cidr), abbrev('192.168.1.5/24'::inet), \
                set_masklen('192.168.1.5/24'::inet, 16), inet_same_family('1.2.3.4'::inet, '::1'::inet), \
                inet_merge('192.168.1.5/24'::inet, '192.168.2.5/24'::inet), \
                trunc('08:00:2b:01:02:03'::macaddr), trunc('08:00:2b:01:02:03:04:05'::macaddr8), \
                macaddr8_set7bit('00:00:2b:01:02:03:04:05'::macaddr8)",
    );
    assert_eq!(
        data_rows(&bytes),
        [
            "4|192.168.1.5|24|192.168.1.255/24|255.255.255.0|0.0.0.255|192.168.1.0/24|10.1/16|192.168.1.5/24|192.168.1.5/16|f|192.168.0.0/22|08:00:2b:00:00:00|08:00:2b:00:00:00:00:00|02:00:2b:01:02:03:04:05"
        ]
    );
}

#[test]
fn network_types_survive_restart() {
    let config = test_config("network-durable");
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        run_with(
            &mut e,
            &mut b,
            "CREATE TABLE nd (a inet, c cidr, m macaddr, m8 macaddr8)",
        );
        run_with(
            &mut e,
            &mut b,
            "INSERT INTO nd VALUES ('2001:db8::1/64','10.0.0.0/8','08:00:2b:01:02:03','08:00:2b:ff:fe:01:02:03')",
        );
    }
    // The values survive WAL replay byte-for-byte (the rowenc codec).
    let mut b = Budget::new(1 << 25);
    let mut e = Engine::new(&config, &mut b).unwrap();
    let bytes = run_with(&mut e, &mut b, "SELECT a, c, m, m8 FROM nd");
    assert_eq!(
        data_rows(&bytes),
        ["2001:db8::1/64|10.0.0.0/8|08:00:2b:01:02:03|08:00:2b:ff:fe:01:02:03"]
    );
}

#[test]
fn network_operators_match_postgres() {
    let (mut e, mut b) = test_engine();
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT '192.168.1.5'::inet << '192.168.1.0/24'::inet, \
                '192.168.1.5'::inet <<= '192.168.1.5/32'::inet, \
                '192.168.1.0/24'::inet >> '192.168.1.5'::inet, \
                '192.168.1.0/24'::cidr && '192.168.1.128/25'::cidr, \
                ~ '192.168.1.5'::inet, \
                '192.168.1.5'::inet & '0.0.0.255'::inet, \
                '192.168.1.0'::inet | '0.0.0.5'::inet, \
                '192.168.1.5'::inet + 10, \
                '192.168.1.5'::inet - 10, \
                '192.168.1.20'::inet - '192.168.1.5'::inet",
    );
    assert_eq!(
        data_rows(&bytes),
        ["t|t|t|t|63.87.254.250|0.0.0.5|192.168.1.5|192.168.1.15|192.168.0.251|15"]
    );
}

#[test]
fn fetch_first_and_with_ties() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE ft (id int, v int)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO ft VALUES (1,10),(2,10),(3,20),(4,20),(5,30),(6,30)",
    );
    // FETCH FIRST n ROWS ONLY == LIMIT n.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT id FROM ft ORDER BY id FETCH FIRST 2 ROWS ONLY",
    );
    assert_eq!(data_rows(&bytes), ["1", "2"]);
    // FETCH FIRST ROW ONLY (count defaults to 1).
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT id FROM ft ORDER BY id FETCH FIRST ROW ONLY",
    );
    assert_eq!(data_rows(&bytes), ["1"]);
    // OFFSET ... FETCH NEXT.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT v FROM ft ORDER BY v OFFSET 1 ROWS FETCH NEXT 2 ROWS ONLY",
    );
    assert_eq!(data_rows(&bytes), ["10", "20"]);
    // WITH TIES: the first row plus every row tying on the ORDER BY key.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT v FROM ft ORDER BY v FETCH FIRST 1 ROWS WITH TIES",
    );
    assert_eq!(data_rows(&bytes), ["10", "10"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT v FROM ft ORDER BY v FETCH FIRST 3 ROWS WITH TIES",
    );
    assert_eq!(data_rows(&bytes), ["10", "10", "20", "20"]);
    // WITH TIES requires ORDER BY (42601).
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT v FROM ft FETCH FIRST 2 ROWS WITH TIES",
    );
    assert!(
        String::from_utf8_lossy(&bytes).contains("42601"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    // WITH TIES over a grouped/aggregate query.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT v, count(*) FROM ft GROUP BY v ORDER BY count(*) FETCH FIRST 1 ROWS WITH TIES",
    );
    assert_eq!(data_rows(&bytes), ["10|2", "20|2", "30|2"]);
    // WITH TIES over a UNION.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT v FROM ft UNION ALL SELECT v FROM ft ORDER BY v FETCH FIRST 1 ROWS WITH TIES",
    );
    assert_eq!(data_rows(&bytes), ["10", "10", "10", "10"]);
}

#[test]
fn domains_enforce_and_report() {
    let (mut e, mut b) = test_engine();
    run_with(
        &mut e,
        &mut b,
        "CREATE DOMAIN posint AS int CHECK (VALUE > 0)",
    );
    run_with(
        &mut e,
        &mut b,
        "CREATE DOMAIN email AS text NOT NULL CHECK (VALUE LIKE '%@%')",
    );
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE dt (id posint DEFAULT 1, addr email)",
    );
    run_with(&mut e, &mut b, "INSERT INTO dt VALUES (5, 'a@b.com')");
    run_with(&mut e, &mut b, "INSERT INTO dt (addr) VALUES ('x@y.com')"); // id defaults to 1
    // pg_typeof: the domain on a bare column, the base through an expression.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT pg_typeof(id), pg_typeof(addr), pg_typeof(id + 1) FROM dt WHERE id = 5",
    );
    assert_eq!(data_rows(&bytes), ["posint|email|integer"]);
    let bytes = run_with(&mut e, &mut b, "SELECT id, addr FROM dt ORDER BY id");
    assert_eq!(data_rows(&bytes), ["1|x@y.com", "5|a@b.com"]);
    // Constraint violations.
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "INSERT INTO dt VALUES (-1, 'a@b.com')"
        ))
        .contains("23514")
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "INSERT INTO dt VALUES (5, 'bad')"
        ))
        .contains("23514")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO dt VALUES (5, NULL)"))
            .contains("23502")
    );
    // DROP RESTRICT fails with a dependent column.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "DROP DOMAIN posint")).contains("2BP01")
    );
    // Unknown type name.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "CREATE TABLE bad (a nope)"))
            .contains("42704")
    );

    // Explicit casts run the same base coercion + constraint path as columns.
    let bytes = run_with(&mut e, &mut b, "SELECT 5::posint, pg_typeof(5::posint)");
    assert_eq!(data_rows(&bytes), ["5|posint"]);
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT 0::posint")).contains("23514")
    );

    // A domain over a domain keeps the parent constraint chain and inherited
    // default, while adding its own rules. Arrays validate every element.
    run_with(
        &mut e,
        &mut b,
        "CREATE DOMAIN smallpos AS posint DEFAULT 7 CHECK (VALUE < 10)",
    );
    let bytes = run_with(
        &mut e,
        &mut b,
        "CREATE TABLE nested (a smallpos, xs smallpos[])",
    );
    assert!(
        String::from_utf8_lossy(&bytes).contains("CREATE TABLE"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let bytes = run_with(
        &mut e,
        &mut b,
        "INSERT INTO nested VALUES (DEFAULT, ARRAY[1,2,9]::smallpos[])",
    );
    assert!(
        String::from_utf8_lossy(&bytes).contains("INSERT 0 1"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT a, xs, pg_typeof(a), pg_typeof(xs) FROM nested",
    );
    assert_eq!(data_rows(&bytes), ["7|{1,2,9}|smallpos|smallpos[]"]);
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT ARRAY[1,0]::smallpos[]"))
            .contains("23514")
    );
    run_with(
        &mut e,
        &mut b,
        "UPDATE nested SET xs = ARRAY[3,4]::smallpos[]",
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT xs FROM nested")),
        ["{3,4}"]
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "ALTER DOMAIN smallpos ADD CHECK (VALUE < 8)"
        ))
        .contains("2BP01")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT ARRAY[1,10]::smallpos[]"))
            .contains("23514")
    );

    // ALTER validates stored scalar values before installing a stronger rule.
    run_with(&mut e, &mut b, "CREATE DOMAIN capped AS int");
    run_with(&mut e, &mut b, "CREATE TABLE capped_t (v capped)");
    run_with(&mut e, &mut b, "INSERT INTO capped_t VALUES (8)");
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "ALTER DOMAIN capped ADD CHECK (VALUE < 5)"
        ))
        .contains("23514")
    );
    // The failed ALTER was rolled back, not left half-installed.
    run_with(&mut e, &mut b, "INSERT INTO capped_t VALUES (9)");

    // Successful ALTER actions also carry compact transaction inverses. This
    // exercises nullability, default, added-check, and dropped-check rollback
    // without embedding a whole domain catalog in every undo entry.
    let bytes = run_with(
        &mut e,
        &mut b,
        "BEGIN;\
         ALTER DOMAIN capped SET NOT NULL;\
         ALTER DOMAIN capped SET DEFAULT 4;\
         ALTER DOMAIN capped ADD CHECK (VALUE < 10);\
         ROLLBACK;\
         INSERT INTO capped_t VALUES (12);\
         INSERT INTO capped_t VALUES (NULL);\
         SELECT v FROM capped_t ORDER BY v NULLS LAST",
    );
    assert_eq!(data_rows(&bytes), ["8", "9", "12", "NULL"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "BEGIN;\
         ALTER DOMAIN posint DROP CONSTRAINT posint_check;\
         ROLLBACK;\
         SELECT 0::posint",
    );
    assert!(String::from_utf8_lossy(&bytes).contains("23514"));
}

#[test]
fn domains_survive_restart() {
    let config = test_config("domain-durable");
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        run_with(
            &mut e,
            &mut b,
            "CREATE DOMAIN posint AS int NOT NULL CHECK (VALUE > 0) CHECK (VALUE < 100)",
        );
        run_with(&mut e, &mut b, "CREATE TABLE dt (a posint DEFAULT 7)");
        run_with(&mut e, &mut b, "INSERT INTO dt VALUES (42)");
        e.commit_wal();
    }
    // WAL replay: the domain and its column identity survive.
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        let bytes = run_with(&mut e, &mut b, "SELECT pg_typeof(a), a FROM dt");
        assert_eq!(data_rows(&bytes), ["posint|42"]);
        // The domain still enforces after replay.
        assert!(
            String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO dt VALUES (0)"))
                .contains("23514")
        );
        assert!(
            String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO dt VALUES (NULL)"))
                .contains("23502")
        );
        // The domain default is baked into the column.
        run_with(&mut e, &mut b, "INSERT INTO dt DEFAULT VALUES");
        let bytes = run_with(&mut e, &mut b, "SELECT a FROM dt ORDER BY a");
        assert_eq!(data_rows(&bytes), ["7", "42"]);
    }
}

#[test]
fn enums_order_and_enforce() {
    let (mut e, mut b) = test_engine();
    run_with(
        &mut e,
        &mut b,
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')",
    );
    run_with(
        &mut e,
        &mut b,
        "CREATE TABLE et (id int, m mood, moods mood[])",
    );
    let bytes = run_with(
        &mut e,
        &mut b,
        "CREATE TABLE enum_defaults (\
             m mood DEFAULT 'ok',\
             moods mood[] DEFAULT ARRAY['sad','happy']::mood[]\
         )",
    );
    assert!(
        String::from_utf8_lossy(&bytes).contains("CREATE TABLE"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let bytes = run_with(&mut e, &mut b, "INSERT INTO enum_defaults DEFAULT VALUES");
    assert!(
        String::from_utf8_lossy(&bytes).contains("INSERT 0 1"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT m, moods FROM enum_defaults"
        )),
        ["ok|{sad,happy}"]
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "CREATE TABLE invalid_enum_default (m mood DEFAULT 'missing')"
        ))
        .contains("22P02")
    );
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO et VALUES \
         (1,'happy',ARRAY['happy']::mood[]),\
         (2,'sad',ARRAY['sad','happy']::mood[]),\
         (3,'ok',ARRAY['ok']::mood[])",
    );
    // Ordering follows definition order, not label text.
    let bytes = run_with(&mut e, &mut b, "SELECT id FROM et ORDER BY m, id");
    assert_eq!(data_rows(&bytes), ["2", "3", "1"]);
    // pg_typeof reports the enum; comparison uses the sort order.
    let bytes = run_with(&mut e, &mut b, "SELECT pg_typeof(m) FROM et WHERE id = 1");
    assert_eq!(data_rows(&bytes), ["mood"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT id FROM et WHERE m > 'sad' ORDER BY id",
    );
    assert_eq!(data_rows(&bytes), ["1", "3"]);
    // An invalid label is 22P02, on write and on cast.
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "INSERT INTO et VALUES (9,'nope','{}'::mood[])"
        ))
        .contains("22P02")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT 'nope'::mood")).contains("22P02")
    );
    // ADD VALUE BEFORE inserts between neighbours; the new order is respected.
    run_with(
        &mut e,
        &mut b,
        "ALTER TYPE mood ADD VALUE 'meh' BEFORE 'ok'",
    );
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO et VALUES (4,'meh',ARRAY['meh']::mood[])",
    );
    let bytes = run_with(&mut e, &mut b, "SELECT id FROM et ORDER BY m, id");
    assert_eq!(data_rows(&bytes), ["2", "4", "3", "1"]);
    // A duplicate label errors 42710; IF NOT EXISTS is a no-op.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "ALTER TYPE mood ADD VALUE 'ok'"))
            .contains("42710")
    );
    // DROP RESTRICT fails while a column depends on the enum.
    assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "DROP TYPE mood")).contains("2BP01"));
    // Renames rewrite inline scalar/array labels while preserving sort order,
    // then move the type identity (including its generated array type).
    run_with(
        &mut e,
        &mut b,
        "ALTER TYPE mood RENAME VALUE 'sad' TO 'blue'",
    );
    let bytes = run_with(&mut e, &mut b, "SELECT m, moods FROM et WHERE id=2");
    assert_eq!(data_rows(&bytes), ["blue|{blue,happy}"]);
    run_with(&mut e, &mut b, "ALTER TYPE mood RENAME TO feeling");
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT pg_typeof(m), pg_typeof(moods), m, moods FROM et WHERE id=2",
    );
    assert_eq!(data_rows(&bytes), ["feeling|feeling[]|blue|{blue,happy}"]);
    run_with(
        &mut e,
        &mut b,
        "UPDATE et SET moods = ARRAY['meh']::feeling[] WHERE id = 4",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT moods FROM et WHERE id = 4"
        )),
        ["{meh}"]
    );

    // Catalog changes and the row rewrite are ordinary transactional work:
    // every inverse is compact, savepoint-safe, and restores the prior names.
    let bytes = run_with(
        &mut e,
        &mut b,
        "BEGIN;\
         ALTER TYPE feeling ADD VALUE 'ecstatic';\
         ALTER TYPE feeling RENAME VALUE 'blue' TO 'azure';\
         ALTER TYPE feeling RENAME TO emotion;\
         ROLLBACK;\
         SELECT pg_typeof(m), m, moods FROM et WHERE id=2",
    );
    assert_eq!(data_rows(&bytes), ["feeling|blue|{blue,happy}"]);
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT 'ecstatic'::feeling"))
            .contains("22P02")
    );
    run_with(
        &mut e,
        &mut b,
        "CREATE TYPE array_only AS ENUM ('x');\
         CREATE TABLE enum_array_only (values array_only[])",
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "DROP TYPE array_only"))
            .contains("2BP01")
    );
}

#[test]
fn enums_survive_restart() {
    let config = test_config("enum-durable");
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        run_with(
            &mut e,
            &mut b,
            "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')",
        );
        run_with(
            &mut e,
            &mut b,
            "ALTER TYPE mood ADD VALUE 'meh' BEFORE 'ok'",
        );
        run_with(&mut e, &mut b, "CREATE TABLE et (id int, m mood)");
        run_with(
            &mut e,
            &mut b,
            "INSERT INTO et VALUES (1,'happy'),(2,'meh'),(3,'sad')",
        );
        run_with(&mut e, &mut b, "ALTER TYPE mood RENAME TO feeling");
        e.commit_wal();
    }
    // WAL replay: the enum, its rename, added value, ordering, and column
    // identity survive, including through grouped projection's schema lookup.
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        let bytes = run_with(&mut e, &mut b, "SELECT id FROM et ORDER BY m, id");
        assert_eq!(data_rows(&bytes), ["3", "2", "1"]);
        let bytes = run_with(&mut e, &mut b, "SELECT pg_typeof(m) FROM et WHERE id = 1");
        assert_eq!(data_rows(&bytes), ["feeling"]);
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT pg_typeof(m), string_agg(id::text, ',' ORDER BY id) \
             FROM et GROUP BY m ORDER BY m",
        );
        assert_eq!(data_rows(&bytes), ["feeling|3", "feeling|2", "feeling|1"]);
        // Still enforces its labels after replay.
        assert!(
            String::from_utf8_lossy(&run_with(
                &mut e,
                &mut b,
                "INSERT INTO et VALUES (9,'bogus')"
            ))
            .contains("22P02")
        );
        // The added value is usable.
        run_with(&mut e, &mut b, "INSERT INTO et VALUES (4,'ok'::feeling)");
        let bytes = run_with(&mut e, &mut b, "SELECT id FROM et ORDER BY m, id");
        assert_eq!(data_rows(&bytes), ["3", "2", "4", "1"]);
    }
}

#[test]
fn user_type_schema_identity_survives_restart() {
    let config = test_config("user-type-schema-durable");
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        run_with(&mut e, &mut b, "CREATE SCHEMA first; CREATE SCHEMA second");
        run_with(
            &mut e,
            &mut b,
            "CREATE DOMAIN first.measure AS int CHECK (VALUE > 0);\
             CREATE DOMAIN second.measure AS int CHECK (VALUE < 0);\
             CREATE DOMAIN first.small AS first.measure CHECK (VALUE < 10);\
             CREATE TYPE first.state AS ENUM ('red');\
             CREATE TYPE second.state AS ENUM ('blue')",
        );
        let bytes = run_with(
            &mut e,
            &mut b,
            "CREATE TABLE typed (\
                 positive first.small,\
                 negative second.measure,\
                 positives first.measure[],\
                 negatives second.measure[],\
                 signal first.state,\
                 signals second.state[]\
             )",
        );
        assert!(
            String::from_utf8_lossy(&bytes).contains("CREATE TABLE"),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let bytes = run_with(
            &mut e,
            &mut b,
            "INSERT INTO typed VALUES (\
                 5, -5,\
                 ARRAY[1,2]::first.measure[],\
                 ARRAY[-1,-2]::second.measure[],\
                 'red', ARRAY['blue']::second.state[]\
             )",
        );
        assert!(
            String::from_utf8_lossy(&bytes).contains("INSERT 0 1"),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let bytes = run_with(&mut e, &mut b, "ALTER TYPE first.state RENAME TO signal");
        assert!(
            String::from_utf8_lossy(&bytes).contains("ALTER TYPE"),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT typname FROM pg_type WHERE typname IN ('signal','state') ORDER BY typname",
        );
        assert_eq!(data_rows(&bytes), ["signal", "state"]);
        e.commit_wal();
    }
    {
        let mut b = Budget::new(1 << 25);
        let mut e = Engine::new(&config, &mut b).unwrap();
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT positive, negative, positives, negatives, signal, signals FROM typed",
        );
        assert_eq!(data_rows(&bytes), ["5|-5|{1,2}|{-1,-2}|red|{blue}"]);
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT typname FROM pg_type WHERE typname IN ('signal','state') ORDER BY typname",
        );
        assert_eq!(data_rows(&bytes), ["signal", "state"]);
        assert!(
            String::from_utf8_lossy(&run_with(
                &mut e,
                &mut b,
                "INSERT INTO typed VALUES (\
                    -1, 1, '{}', '{}', 'red', ARRAY['blue']::second.state[]\
                 )"
            ))
            .contains("23514")
        );
        let bytes = run_with(&mut e, &mut b, "SELECT 'blue'::first.signal");
        assert!(
            String::from_utf8_lossy(&bytes).contains("22P02"),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
    }
}

#[test]
fn lateral_joins() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE lt (id int, n int)");
    run_with(&mut e, &mut b, "INSERT INTO lt VALUES (1,2),(2,3),(3,0)");
    run_with(&mut e, &mut b, "CREATE TABLE lu (tid int, v text)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO lu VALUES (1,'a'),(1,'b'),(2,'c')",
    );
    // A FROM-less lateral body projects an outer expression per row.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT id, d FROM lt, LATERAL (SELECT lt.n*2 AS d) s ORDER BY id",
    );
    assert_eq!(data_rows(&bytes), ["1|4", "2|6", "3|0"]);
    // CROSS JOIN LATERAL over a correlated subquery (correlation in WHERE);
    // rows with no match drop out.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT t.id, s.v FROM lt t CROSS JOIN LATERAL (SELECT v FROM lu WHERE lu.tid=t.id) s ORDER BY t.id, s.v",
    );
    assert_eq!(data_rows(&bytes), ["1|a", "1|b", "2|c"]);
    // LEFT JOIN LATERAL preserves a left row whose lateral side is empty.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT t.id, s.v FROM lt t LEFT JOIN LATERAL (SELECT v FROM lu WHERE lu.tid=t.id) s ON true ORDER BY t.id, s.v",
    );
    assert_eq!(data_rows(&bytes), ["1|a", "1|b", "2|c", "3|NULL"]);
    // An aggregate inside the lateral body — one row per outer row, 0 for none.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT t.id, s.c FROM lt t, LATERAL (SELECT count(*) AS c FROM lu WHERE lu.tid=t.id) s ORDER BY t.id",
    );
    assert_eq!(data_rows(&bytes), ["1|2", "2|1", "3|0"]);
    // A set-returning function taking an outer argument; an empty series (n=0)
    // contributes no rows.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT id, g FROM lt, LATERAL generate_series(1, lt.n) g ORDER BY id, g",
    );
    assert_eq!(data_rows(&bytes), ["1|1", "1|2", "2|1", "2|2", "2|3"]);
    // The LATERAL keyword is optional for functions: arguments may reference
    // preceding FROM items, and WITH ORDINALITY restarts for every left row.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT id, g, ordinality FROM lt, generate_series(1, lt.n) WITH ORDINALITY g ORDER BY id, g",
    );
    assert_eq!(
        data_rows(&bytes),
        ["1|1|1", "1|2|2", "2|1|1", "2|2|2", "2|3|3"]
    );
    // A table-function argument inside an ARRAY subquery can reference the
    // enclosing query. This is the shape emitted by psql's describe queries.
    run_with(&mut e, &mut b, "CREATE TABLE la (id int, options text[])");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO la VALUES (1, ARRAY['a','b']), (2, NULL), (3, ARRAY['c'])",
    );
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT id, ARRAY(SELECT 'x.' || option FROM unnest(la.options) option) FROM la ORDER BY id",
    );
    assert_eq!(data_rows(&bytes), ["1|{x.a,x.b}", "2|{}", "3|{x.c}"]);
    // Two lateral items, the second referencing the first's output.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT t.id, a.g, b.d FROM lt t, LATERAL generate_series(1,t.n) a(g), LATERAL (SELECT a.g*10 AS d) b ORDER BY t.id, a.g",
    );
    assert_eq!(
        data_rows(&bytes),
        ["1|1|10", "1|2|20", "2|1|10", "2|2|20", "2|3|30"]
    );
    // RIGHT JOIN LATERAL is rejected loudly.
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "SELECT * FROM lt t RIGHT JOIN LATERAL (SELECT 1) s ON true"
        ))
        .contains("0A000")
    );
}

#[test]
fn drop_table_frees_the_name() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (id int)");
    run_with(&mut e, &mut b, "DROP TABLE t");
    let bytes = run_with(&mut e, &mut b, "CREATE TABLE t (id int)");
    assert_eq!(message_types(&bytes), [b'C']);
    let bytes = run_with(&mut e, &mut b, "DROP TABLE IF EXISTS never_was");
    assert_eq!(message_types(&bytes), [b'N', b'C']);
}

#[test]
fn correlated_scalar_subquery_in_projection() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (a int, b int)");
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,10),(2,20),(3,30)");
    // For each row, count rows with a smaller b (a running rank).
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT a, (SELECT count(*) FROM t AS x WHERE x.b < t.b) AS rnk FROM t ORDER BY a",
    );
    assert_eq!(data_rows(&bytes), ["1|0", "2|1", "3|2"]);
}

#[test]
fn correlated_scalar_subquery_streaming() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (a int, b int)");
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,10),(2,20)");
    // No ORDER BY: streaming path (scan order is unspecified, so compare
    // as a set).
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT a, (SELECT count(*) FROM t AS x WHERE x.b <= t.b) FROM t",
    );
    let mut got = data_rows(&bytes);
    got.sort();
    assert_eq!(got, ["1|1", "2|2"]);
}

#[test]
fn where_filters_before_correlated_projection() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE outer_rows (id integer)",
    );
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE inner_rows (outer_id integer, value integer)",
    );
    run_with(
        &mut engine,
        &mut budget,
        "INSERT INTO outer_rows VALUES (1), (2)",
    );
    run_with(
        &mut engine,
        &mut budget,
        "INSERT INTO inner_rows VALUES (1, 10), (1, 11), (2, 20)",
    );

    // The rejected outer row has a multi-row scalar subquery. PostgreSQL never
    // evaluates that select-list expression because WHERE removes the row first.
    let output = run_with(
        &mut engine,
        &mut budget,
        "SELECT (SELECT value FROM inner_rows WHERE outer_id = o.id)
         FROM outer_rows AS o
         WHERE o.id = 2",
    );
    let rows = data_rows(&output);
    assert_eq!(rows, ["20"], "{}", String::from_utf8_lossy(&output));
    let ordered = run_with(
        &mut engine,
        &mut budget,
        "SELECT (SELECT value FROM inner_rows WHERE outer_id = o.id)
         FROM outer_rows AS o
         WHERE o.id = 2
         ORDER BY o.id",
    );
    assert_eq!(
        data_rows(&ordered),
        ["20"],
        "{}",
        String::from_utf8_lossy(&ordered)
    );
}

#[test]
fn exists_correlated_in_where() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (a int)");
    run_with(&mut e, &mut b, "CREATE TABLE u (k int)");
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (1),(2),(3)");
    run_with(&mut e, &mut b, "INSERT INTO u VALUES (2),(3),(4)");
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.k = t.a) ORDER BY a",
    );
    assert_eq!(data_rows(&bytes), ["2", "3"]);
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT a FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE u.k = t.a) ORDER BY a",
    );
    assert_eq!(data_rows(&bytes), ["1"]);
}

#[test]
fn exists_uncorrelated() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (a int)");
    run_with(&mut e, &mut b, "CREATE TABLE u (k int)");
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (1),(2)");
    // u empty: EXISTS is false for all rows, NOT EXISTS true for all.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u)",
    );
    assert_eq!(data_rows(&bytes), Vec::<String>::new());
    run_with(&mut e, &mut b, "INSERT INTO u VALUES (9)");
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u) ORDER BY a",
    );
    assert_eq!(data_rows(&bytes), ["1", "2"]);
}

#[test]
fn for_update_locking_clause() {
    // Row-locking clauses return the query's rows unchanged in a single
    // session, and enforce PostgreSQL's analysis-time restrictions.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE lk (id int, v int)");
    run_with(&mut e, &mut b, "INSERT INTO lk VALUES (1,10),(2,20),(3,30)");

    // Each strength + OF / NOWAIT / SKIP LOCKED returns the same rows.
    for sql in [
        "SELECT id FROM lk ORDER BY id FOR UPDATE",
        "SELECT id FROM lk ORDER BY id FOR NO KEY UPDATE",
        "SELECT id FROM lk ORDER BY id FOR SHARE",
        "SELECT id FROM lk ORDER BY id FOR KEY SHARE",
        "SELECT id FROM lk ORDER BY id FOR UPDATE OF lk",
        "SELECT id FROM lk ORDER BY id FOR UPDATE NOWAIT",
        "SELECT id FROM lk ORDER BY id FOR UPDATE SKIP LOCKED",
        "SELECT id FROM lk t1 ORDER BY id FOR UPDATE OF t1",
    ] {
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, sql)),
            ["1", "2", "3"],
            "{sql}"
        );
    }
    // A FROM-less SELECT may carry the clause (it locks nothing).
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT 1 FOR UPDATE")),
        ["1"]
    );

    // Analysis-time restrictions, each with the clause's own keyword and SQLSTATE.
    let err = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    assert!(
        err(&run_with(
            &mut e,
            &mut b,
            "SELECT count(*) FROM lk FOR UPDATE"
        ))
        .contains("0A000")
    );
    assert!(
        err(&run_with(
            &mut e,
            &mut b,
            "SELECT id FROM lk GROUP BY id FOR UPDATE"
        ))
        .contains("0A000")
    );
    assert!(
        err(&run_with(
            &mut e,
            &mut b,
            "SELECT DISTINCT id FROM lk FOR UPDATE"
        ))
        .contains("0A000")
    );
    assert!(
        err(&run_with(
            &mut e,
            &mut b,
            "SELECT id FROM lk UNION SELECT v FROM lk FOR UPDATE"
        ))
        .contains("0A000")
    );
    assert!(
        err(&run_with(
            &mut e,
            &mut b,
            "SELECT id, row_number() OVER () FROM lk FOR UPDATE"
        ))
        .contains("0A000")
    );
    // OF must name a relation in the FROM clause (42P01); an alias hides the name.
    assert!(
        err(&run_with(
            &mut e,
            &mut b,
            "SELECT id FROM lk FOR UPDATE OF nope"
        ))
        .contains("42P01")
    );
    assert!(
        err(&run_with(
            &mut e,
            &mut b,
            "SELECT id FROM lk x FOR UPDATE OF lk"
        ))
        .contains("42P01")
    );
    // CTE/view expansion must preserve the main query's locking clause.
    assert!(
        err(&run_with(
            &mut e,
            &mut b,
            "WITH rows AS (SELECT id FROM lk) SELECT count(*) FROM rows FOR UPDATE"
        ))
        .contains("0A000")
    );
}

#[test]
fn row_lock_compatibility_nowait_skip_and_wait_resume() {
    let (mut engine, mut budget) = test_engine();
    let mut owner = TxnState::new(&mut budget, 256).unwrap();
    let mut waiter = TxnState::new(&mut budget, 256).unwrap();
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "CREATE TABLE row_lock_test (id int primary key, value int)",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "INSERT INTO row_lock_test VALUES (1, 10), (2, 20)",
    );
    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    let locked = run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "SELECT id FROM row_lock_test WHERE id = 1 FOR UPDATE",
    );
    assert!(locked.contains("SELECT 1"), "{locked}");

    let nowait = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "SELECT id FROM row_lock_test FOR UPDATE NOWAIT",
    );
    assert!(nowait.contains("55P03"), "{nowait}");
    let skipped = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "SELECT id FROM row_lock_test ORDER BY id FOR UPDATE SKIP LOCKED",
    );
    assert!(
        skipped
            .as_bytes()
            .windows(5)
            .any(|window| window == [0, 0, 0, 1, b'2']),
        "{skipped}"
    );
    assert!(
        !skipped
            .as_bytes()
            .windows(5)
            .any(|window| window == [0, 0, 0, 1, b'1']),
        "{skipped}"
    );

    let mut send = crate::mem::FixedBuf::new(&mut budget, "lock wait send", 1 << 18).unwrap();
    let arena = Arena::new(&mut budget, "lock wait sql", 1 << 18).unwrap();
    let mut pool = test_pool(&mut budget);
    let mut guc = GucState::new();
    let mut cursors = test_cursors(&mut budget);
    let sql = "UPDATE row_lock_test SET value = 11 WHERE id = 1";
    let status = engine
        .execute_simple(
            sql,
            &arena,
            &mut waiter,
            &mut pool,
            &mut cursors,
            &mut guc,
            &mut Responder::new(&mut send),
            2,
        )
        .unwrap();
    assert_eq!(
        status,
        ExecutionStatus::Blocked {
            completed_statements: 0,
            output_mark: 0,
        }
    );
    assert!(send.is_empty(), "a parked statement emits no wire output");

    let timed_out = engine
        .execute_simple_from(
            sql,
            0,
            &arena,
            &mut waiter,
            &mut pool,
            &mut cursors,
            &mut guc,
            &mut Responder::new(&mut send),
            2,
            true,
        )
        .unwrap();
    assert_eq!(timed_out, ExecutionStatus::Complete);
    let timeout_output = String::from_utf8_lossy(send.readable());
    assert!(timeout_output.contains("55P03"), "{timeout_output}");
    assert!(
        timeout_output.contains("canceling statement due to lock timeout"),
        "{timeout_output}"
    );
    send.clear();

    let mut resume_waiter = TxnState::new(&mut budget, 256).unwrap();
    let parked_again = engine
        .execute_simple(
            sql,
            &arena,
            &mut resume_waiter,
            &mut pool,
            &mut cursors,
            &mut guc,
            &mut Responder::new(&mut send),
            3,
        )
        .unwrap();
    assert!(matches!(parked_again, ExecutionStatus::Blocked { .. }));

    run_txn(&mut engine, &mut budget, &mut owner, "COMMIT");
    let resumed = engine
        .execute_simple_from(
            sql,
            0,
            &arena,
            &mut resume_waiter,
            &mut pool,
            &mut cursors,
            &mut guc,
            &mut Responder::new(&mut send),
            3,
            false,
        )
        .unwrap();
    assert_eq!(resumed, ExecutionStatus::Complete);
    assert!(String::from_utf8_lossy(send.readable()).contains("UPDATE 1"));
}

#[test]
fn table_lock_modes_nowait_and_wait_match_postgresql_matrix() {
    let (mut engine, mut budget) = test_engine();
    let mut owner = TxnState::new(&mut budget, 256).unwrap();
    let mut waiter = TxnState::new(&mut budget, 256).unwrap();
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "CREATE TABLE table_lock_test (id int)",
    );
    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    assert!(
        run_txn(
            &mut engine,
            &mut budget,
            &mut owner,
            "LOCK TABLE table_lock_test IN SHARE MODE",
        )
        .contains("LOCK TABLE")
    );
    run_txn(&mut engine, &mut budget, &mut waiter, "BEGIN");
    let conflict = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "LOCK TABLE table_lock_test IN ROW EXCLUSIVE MODE NOWAIT",
    );
    assert!(conflict.contains("55P03"), "{conflict}");
    run_txn(&mut engine, &mut budget, &mut waiter, "ROLLBACK");
    run_txn(&mut engine, &mut budget, &mut waiter, "BEGIN");
    let blocked = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "LOCK TABLE table_lock_test IN ROW EXCLUSIVE MODE",
    );
    assert!(blocked.is_empty(), "parked LOCK emits nothing: {blocked}");
    run_txn(&mut engine, &mut budget, &mut owner, "COMMIT");
    let resumed = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "LOCK TABLE table_lock_test IN ROW EXCLUSIVE MODE",
    );
    assert!(resumed.contains("LOCK TABLE"), "{resumed}");
    run_txn(&mut engine, &mut budget, &mut waiter, "ROLLBACK");

    // The omitted mode is ACCESS EXCLUSIVE, and every spelling parses.
    for mode in [
        "ACCESS SHARE",
        "ROW SHARE",
        "ROW EXCLUSIVE",
        "SHARE UPDATE EXCLUSIVE",
        "SHARE",
        "SHARE ROW EXCLUSIVE",
        "EXCLUSIVE",
        "ACCESS EXCLUSIVE",
    ] {
        run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
        let output = run_txn(
            &mut engine,
            &mut budget,
            &mut owner,
            &format!("LOCK TABLE table_lock_test IN {mode} MODE"),
        );
        assert!(output.contains("LOCK TABLE"), "{mode}: {output}");
        run_txn(&mut engine, &mut budget, &mut owner, "ROLLBACK");
    }

    // Ordinary commands participate in the same relation-lock matrix.
    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "LOCK TABLE table_lock_test IN ACCESS EXCLUSIVE MODE",
    );
    let blocked_select = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "SELECT count(*) FROM table_lock_test",
    );
    assert!(
        blocked_select.is_empty(),
        "ordinary SELECT must wait for ACCESS EXCLUSIVE: {blocked_select}"
    );
    run_txn(&mut engine, &mut budget, &mut owner, "COMMIT");
    let resumed_select = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "SELECT count(*) FROM table_lock_test",
    );
    assert!(resumed_select.contains("SELECT 1"), "{resumed_select}");

    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "LOCK TABLE table_lock_test IN SHARE MODE",
    );
    let blocked_update = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "UPDATE table_lock_test SET id = id",
    );
    assert!(
        blocked_update.is_empty(),
        "ordinary UPDATE must wait for SHARE: {blocked_update}"
    );
    run_txn(&mut engine, &mut budget, &mut owner, "COMMIT");
    let resumed_update = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "UPDATE table_lock_test SET id = id",
    );
    assert!(resumed_update.contains("UPDATE 0"), "{resumed_update}");

    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "LOCK TABLE table_lock_test IN ACCESS SHARE MODE",
    );
    let compatible_update = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "UPDATE table_lock_test SET id = id",
    );
    assert!(
        compatible_update.contains("UPDATE 0"),
        "ACCESS SHARE and ROW EXCLUSIVE are compatible: {compatible_update}"
    );
    run_txn(&mut engine, &mut budget, &mut owner, "ROLLBACK");
}

#[test]
fn rollback_to_savepoint_releases_only_subtransaction_locks() {
    let (mut engine, mut budget) = test_engine();
    let mut owner = TxnState::new(&mut budget, 256).unwrap();
    let mut waiter = TxnState::new(&mut budget, 256).unwrap();
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "CREATE TABLE savepoint_locks (id int primary key)",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "INSERT INTO savepoint_locks VALUES (1)",
    );

    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "LOCK TABLE savepoint_locks IN ACCESS SHARE MODE",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "SAVEPOINT before_share",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "LOCK TABLE savepoint_locks IN SHARE MODE",
    );
    run_txn(&mut engine, &mut budget, &mut waiter, "BEGIN");
    let table_conflict = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "LOCK TABLE savepoint_locks IN ROW EXCLUSIVE MODE NOWAIT",
    );
    assert!(table_conflict.contains("55P03"), "{table_conflict}");
    run_txn(&mut engine, &mut budget, &mut waiter, "ROLLBACK");
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "ROLLBACK TO SAVEPOINT before_share",
    );
    run_txn(&mut engine, &mut budget, &mut waiter, "BEGIN");
    let table_acquired = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "LOCK TABLE savepoint_locks IN ROW EXCLUSIVE MODE NOWAIT",
    );
    assert!(
        table_acquired.contains("LOCK TABLE"),
        "the pre-savepoint ACCESS SHARE remains, but SHARE is released: {table_acquired}"
    );
    run_txn(&mut engine, &mut budget, &mut waiter, "ROLLBACK");

    run_txn(&mut engine, &mut budget, &mut owner, "SAVEPOINT before_row");
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "SELECT id FROM savepoint_locks FOR UPDATE",
    );
    run_txn(&mut engine, &mut budget, &mut waiter, "BEGIN");
    let row_conflict = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "SELECT id FROM savepoint_locks FOR UPDATE NOWAIT",
    );
    assert!(row_conflict.contains("55P03"), "{row_conflict}");
    run_txn(&mut engine, &mut budget, &mut waiter, "ROLLBACK");
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "ROLLBACK TO SAVEPOINT before_row",
    );
    run_txn(&mut engine, &mut budget, &mut waiter, "BEGIN");
    let row_acquired = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "SELECT id FROM savepoint_locks FOR UPDATE NOWAIT",
    );
    assert!(row_acquired.contains("SELECT 1"), "{row_acquired}");
    run_txn(&mut engine, &mut budget, &mut waiter, "ROLLBACK");
    run_txn(&mut engine, &mut budget, &mut owner, "ROLLBACK");
}

#[test]
fn row_lock_deadlock_aborts_one_explicit_transaction_and_wakes_the_other() {
    let (mut engine, mut budget) = test_engine();
    let mut first = TxnState::new(&mut budget, 256).unwrap();
    let mut second = TxnState::new(&mut budget, 256).unwrap();
    run_txn(
        &mut engine,
        &mut budget,
        &mut first,
        "CREATE TABLE deadlock_rows (id int primary key, value int);
         INSERT INTO deadlock_rows VALUES (1, 10), (2, 20)",
    );
    run_txn(&mut engine, &mut budget, &mut first, "BEGIN");
    run_txn(&mut engine, &mut budget, &mut second, "BEGIN");
    run_txn(
        &mut engine,
        &mut budget,
        &mut first,
        "SELECT id FROM deadlock_rows WHERE id = 1 FOR UPDATE",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut second,
        "SELECT id FROM deadlock_rows WHERE id = 2 FOR UPDATE",
    );
    let parked = run_txn(
        &mut engine,
        &mut budget,
        &mut first,
        "UPDATE deadlock_rows SET value = 21 WHERE id = 2",
    );
    assert!(parked.is_empty(), "{parked}");
    let victim = run_txn(
        &mut engine,
        &mut budget,
        &mut second,
        "UPDATE deadlock_rows SET value = 11 WHERE id = 1",
    );
    assert!(victim.contains("40P01"), "{victim}");
    assert_eq!(second.status_byte(), b'E');
    let resumed = run_txn(
        &mut engine,
        &mut budget,
        &mut first,
        "UPDATE deadlock_rows SET value = 21 WHERE id = 2",
    );
    assert!(resumed.contains("UPDATE 1"), "{resumed}");
    run_txn(&mut engine, &mut budget, &mut first, "COMMIT");
    run_txn(&mut engine, &mut budget, &mut second, "ROLLBACK");
}

#[test]
fn unique_key_writer_waits_then_rechecks_the_committed_outcome() {
    let (mut engine, mut budget) = test_engine();
    let mut owner = TxnState::new(&mut budget, 256).unwrap();
    let mut waiter = TxnState::new(&mut budget, 256).unwrap();
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "CREATE TABLE unique_wait (id int primary key)",
    );
    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "INSERT INTO unique_wait VALUES (1)",
    );
    let blocked = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "INSERT INTO unique_wait VALUES (1)",
    );
    assert!(blocked.is_empty(), "{blocked}");
    run_txn(&mut engine, &mut budget, &mut owner, "COMMIT");
    let duplicate = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "INSERT INTO unique_wait VALUES (1)",
    );
    assert!(duplicate.contains("23505"), "{duplicate}");
}

#[test]
fn parked_statement_rewinds_partial_rows_before_replay() {
    let (mut engine, mut budget) = test_engine();
    let mut owner = TxnState::new(&mut budget, 256).unwrap();
    let mut waiter = TxnState::new(&mut budget, 256).unwrap();
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "CREATE TABLE replay_rows (id int primary key)",
    );
    run_txn(&mut engine, &mut budget, &mut owner, "BEGIN");
    run_txn(
        &mut engine,
        &mut budget,
        &mut owner,
        "INSERT INTO replay_rows VALUES (2)",
    );
    let blocked = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "INSERT INTO replay_rows VALUES (1), (2)",
    );
    assert!(blocked.is_empty(), "{blocked}");
    assert!(
        engine
            .storage
            .table(engine.storage.find_table("public", "replay_rows").unwrap())
            .rows
            .iter()
            .all(|(_, state)| {
                state
                    .pending
                    .last()
                    .is_none_or(|pending| pending.txid != waiter.txid)
            }),
        "the parked statement must leave no partial pending row"
    );
    run_txn(&mut engine, &mut budget, &mut owner, "ROLLBACK");
    let resumed = run_txn(
        &mut engine,
        &mut budget,
        &mut waiter,
        "INSERT INTO replay_rows VALUES (1), (2)",
    );
    assert!(resumed.contains("INSERT 0 2"), "{resumed}");
}

#[test]
fn select_into_creates_table() {
    // SELECT ... INTO table is CREATE TABLE AS spelled the old way.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE si (id int, v int, s text)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO si VALUES (1,10,'a'),(2,20,'b'),(3,30,'c')",
    );

    // Basic projection + WHERE materialize into a new table.
    run_with(&mut e, &mut b, "SELECT id, v INTO t1 FROM si WHERE id < 3");
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT id, v FROM t1 ORDER BY id"
        )),
        ["1|10", "2|20"]
    );

    // INTO TABLE, computed/renamed columns, trailing ORDER BY.
    run_with(
        &mut e,
        &mut b,
        "SELECT id AS k, v*2 AS d INTO TABLE t2 FROM si ORDER BY id",
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT k, d FROM t2 ORDER BY k")),
        ["1|20", "2|40", "3|60"]
    );

    // Re-running into an existing table errors (42P07).
    let err = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    assert!(err(&run_with(&mut e, &mut b, "SELECT id INTO t1 FROM si")).contains("42P07"));

    // INTO inside a subquery is rejected (42601), and never as a bare alias.
    assert!(
        err(&run_with(
            &mut e,
            &mut b,
            "SELECT * FROM (SELECT 1 INTO nope) x"
        ))
        .contains("42601")
    );
    // `into` remains usable as an explicit column alias.
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT 1 AS into")),
        ["1"]
    );
}

#[test]
fn current_setting_reads_gucs() {
    // current_setting(name [, missing_ok]) returns a setting's value as text —
    // the same value SHOW reports — and composes in expressions.
    let (mut e, mut b) = test_engine();
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT current_setting('client_encoding')"
        )),
        ["UTF8"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT current_setting('server_version_num')"
        )),
        ["180004"]
    );
    // Case-insensitive name; composes under another function.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT lower(current_setting('SERVER_ENCODING'))"
        )),
        ["utf8"]
    );
    // Reflects a SET earlier in the same message.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SET search_path = myschema, public; SELECT current_setting('search_path')"
        )),
        ["myschema, public"]
    );
    // Unknown setting: 42704, or NULL with missing_ok = true.
    let err = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    assert!(
        err(&run_with(
            &mut e,
            &mut b,
            "SELECT current_setting('no_such_xyz')"
        ))
        .contains("42704")
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT current_setting('no_such_xyz', true) IS NULL"
        )),
        ["t"]
    );
}

#[test]
fn configuration_changes_follow_transaction_scope() {
    let (mut engine, mut budget) = test_engine();
    let mut guc = GucState::new();

    assert_eq!(
        data_rows(&run_session(
            &mut engine,
            &mut budget,
            &mut guc,
            "SELECT set_config('application_name', 'committed', false), \
                    current_setting('application_name')"
        )),
        ["committed|committed"]
    );
    assert_eq!(
        data_rows(&run_session(
            &mut engine,
            &mut budget,
            &mut guc,
            "SHOW application_name"
        )),
        ["committed"]
    );

    // A session SET rolls back with its transaction; SET LOCAL is visible only
    // until commit and then exposes a preceding session SET.
    assert_eq!(
        data_rows(&run_session(
            &mut engine,
            &mut budget,
            &mut guc,
            "BEGIN; SET application_name = rolled_back; \
             SELECT current_setting('application_name'); ROLLBACK; \
             SHOW application_name"
        )),
        ["rolled_back", "committed"]
    );
    assert_eq!(
        data_rows(&run_session(
            &mut engine,
            &mut budget,
            &mut guc,
            "BEGIN; SET application_name = session_value; \
             SET LOCAL application_name = local_value; \
             SELECT current_setting('application_name'); COMMIT; \
             SHOW application_name"
        )),
        ["local_value", "session_value"]
    );
    // A session assignment after an unrelated LOCAL overlay must not make
    // that overlay survive commit.
    assert_eq!(
        data_rows(&run_session(
            &mut engine,
            &mut budget,
            &mut guc,
            "BEGIN; SET LOCAL search_path = private; \
             SET application_name = mixed_session; COMMIT; \
             SELECT current_setting('application_name'), current_setting('search_path')"
        )),
        ["mixed_session|\"$user\", public"]
    );

    // Savepoint rollback restores both the visible and eventual session value.
    assert_eq!(
        data_rows(&run_session(
            &mut engine,
            &mut budget,
            &mut guc,
            "BEGIN; SAVEPOINT s; SET application_name = doomed; \
             SET LOCAL search_path = private; ROLLBACK TO s; \
             SELECT current_setting('application_name'), current_setting('search_path'); \
             COMMIT"
        )),
        ["mixed_session|\"$user\", public"]
    );

    assert_eq!(
        data_rows(&run_session(
            &mut engine,
            &mut budget,
            &mut guc,
            "RESET application_name; SHOW application_name; \
             SET search_path = private; RESET ALL; \
             SELECT current_setting('search_path')"
        )),
        ["", "\"$user\", public"]
    );

    assert_eq!(
        data_rows(&run_session(
            &mut engine,
            &mut budget,
            &mut guc,
            "SELECT set_config('application_name', 'null-scope', NULL), \
                    current_setting('application_name'); \
             SELECT set_config('application_name', NULL, false), \
                    current_setting('application_name')"
        )),
        ["null-scope|null-scope", "|"]
    );
    assert!(
        String::from_utf8_lossy(&run_session(
            &mut engine,
            &mut budget,
            &mut guc,
            "SELECT set_config(NULL, 'x', false)"
        ))
        .contains("22004")
    );

    // The transaction and GUC state normally persist across protocol messages,
    // not merely across semicolon-separated statements in one message.
    let mut transaction = TxnState::new(&mut budget, 128).unwrap();
    let mut connection_guc = GucState::new();
    run_session_transaction(
        &mut engine,
        &mut budget,
        &mut transaction,
        &mut connection_guc,
        "SET application_name = before",
    );
    run_session_transaction(
        &mut engine,
        &mut budget,
        &mut transaction,
        &mut connection_guc,
        "BEGIN",
    );
    run_session_transaction(
        &mut engine,
        &mut budget,
        &mut transaction,
        &mut connection_guc,
        "SET application_name = inside",
    );
    assert_eq!(
        data_rows(&run_session_transaction(
            &mut engine,
            &mut budget,
            &mut transaction,
            &mut connection_guc,
            "SHOW application_name"
        )),
        ["inside"]
    );
    run_session_transaction(
        &mut engine,
        &mut budget,
        &mut transaction,
        &mut connection_guc,
        "ROLLBACK",
    );
    assert_eq!(
        data_rows(&run_session_transaction(
            &mut engine,
            &mut budget,
            &mut transaction,
            &mut connection_guc,
            "SHOW application_name"
        )),
        ["before"]
    );
}

#[test]
fn fromless_select_with_subquery() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t1 (x int)");
    run_with(&mut e, &mut b, "INSERT INTO t1 VALUES (1),(2),(3)");
    // IN-subquery with SELECT * (single column) in a FROM-less SELECT.
    let bytes = run_with(&mut e, &mut b, "SELECT 1 IN (SELECT * FROM t1)");
    assert_eq!(data_rows(&bytes), ["t"]);
    let bytes = run_with(&mut e, &mut b, "SELECT 9 IN (SELECT * FROM t1)");
    assert_eq!(data_rows(&bytes), ["f"]);
    // Scalar subquery in a FROM-less SELECT.
    let bytes = run_with(&mut e, &mut b, "SELECT (SELECT count(*) FROM t1) AS c");
    assert_eq!(data_rows(&bytes), ["3"]);
    // EXISTS in a FROM-less SELECT.
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT EXISTS (SELECT 1 FROM t1 WHERE x > 2)",
    );
    assert_eq!(data_rows(&bytes), ["t"]);
}

#[test]
fn data_modifying_cte_select_main() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE dc (id int, v text)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO dc VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d')",
    );

    // DELETE ... RETURNING is a relation the main query reads.
    let bytes = run_with(
        &mut e,
        &mut b,
        "WITH m AS (DELETE FROM dc WHERE id <= 2 RETURNING id, v) SELECT id, v FROM m ORDER BY id",
    );
    assert_eq!(data_rows(&bytes), ["1|a", "2|b"]);

    // The command snapshot: the main query reads the base table as it was
    // BEFORE the statement, so the just-deleted rows are still counted.
    let bytes = run_with(
        &mut e,
        &mut b,
        "WITH d AS (DELETE FROM dc WHERE id = 3 RETURNING id) \
         SELECT (SELECT count(*) FROM d) AS deleted, (SELECT count(*) FROM dc) AS still",
    );
    assert_eq!(data_rows(&bytes), ["1|2"]);

    // INSERT ... RETURNING as a relation.
    let bytes = run_with(
        &mut e,
        &mut b,
        "WITH i AS (INSERT INTO dc VALUES (10,'x'),(20,'y') RETURNING id) SELECT sum(id) FROM i",
    );
    assert_eq!(data_rows(&bytes), ["30"]);

    // UPDATE ... RETURNING as a relation; the main query still reads the
    // pre-update value from the base table under the same snapshot.
    let bytes = run_with(
        &mut e,
        &mut b,
        "WITH u AS (UPDATE dc SET v='Z' WHERE id=4 RETURNING v) \
         SELECT (SELECT v FROM u) AS updated, (SELECT v FROM dc WHERE id=4) AS base",
    );
    assert_eq!(data_rows(&bytes), ["Z|d"]);

    // The RETURNING relation honors a CTE column rename list.
    let bytes = run_with(
        &mut e,
        &mut b,
        "WITH r(the_id) AS (DELETE FROM dc WHERE id=10 RETURNING id) SELECT the_id FROM r",
    );
    assert_eq!(data_rows(&bytes), ["10"]);
}

#[test]
fn data_modifying_cte_sees_prior_command_version_inside_transaction() {
    let (mut engine, mut budget) = test_engine();
    let bytes = run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE command_versions (id int PRIMARY KEY, value text);\
         INSERT INTO command_versions VALUES (1, 'committed');\
         BEGIN;\
         UPDATE command_versions SET value = 'first' WHERE id = 1;\
         WITH changed AS (\
             UPDATE command_versions SET value = 'second' WHERE id = 1 RETURNING value\
         )\
         SELECT (SELECT value FROM changed),\
                (SELECT value FROM command_versions WHERE id = 1);\
         COMMIT;\
         SELECT value FROM command_versions WHERE id = 1",
    );
    assert_eq!(
        data_rows(&bytes),
        ["second|first", "second"],
        "the WITH command snapshot must see the preceding command's pending row image"
    );
}

#[test]
fn data_modifying_cte_main_insert() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE dc2 (id int, v text)");
    run_with(&mut e, &mut b, "INSERT INTO dc2 VALUES (1,'a')");
    let bytes = run_with(
        &mut e,
        &mut b,
        "WITH m AS (DELETE FROM dc2 WHERE id=1 RETURNING id, v) INSERT INTO dc2 SELECT id+100, v FROM m",
    );
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("ERROR"), "WITH INSERT failed: {text:?}");
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT id, v FROM dc2 ORDER BY id"
        )),
        ["101|a"]
    );
}

#[test]
fn with_query_ctes_feed_every_data_modification() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE source_rows (id int, value text);\
         CREATE TABLE target_rows (id int PRIMARY KEY, value text);\
         INSERT INTO source_rows VALUES (1,'a'),(2,'b'),(3,'c');\
         INSERT INTO target_rows VALUES (1,'old'),(4,'remove')",
    );

    let inserted = run_with(
        &mut engine,
        &mut budget,
        "WITH picked AS (SELECT id, value FROM source_rows WHERE id IN (2,3)) \
         INSERT INTO target_rows SELECT * FROM picked RETURNING id, value",
    );
    assert_eq!(data_rows(&inserted), ["2|b", "3|c"]);

    let updated = run_with(
        &mut engine,
        &mut budget,
        "WITH picked AS (SELECT id, value FROM source_rows WHERE id=1) \
         UPDATE target_rows SET value=picked.value FROM picked \
         WHERE target_rows.id=picked.id RETURNING target_rows.id, target_rows.value",
    );
    assert_eq!(data_rows(&updated), ["1|a"]);

    let deleted = run_with(
        &mut engine,
        &mut budget,
        "WITH picked AS (SELECT 4 AS id) \
         DELETE FROM target_rows USING picked WHERE target_rows.id=picked.id \
         RETURNING target_rows.id",
    );
    assert_eq!(data_rows(&deleted), ["4"]);

    run_with(
        &mut engine,
        &mut budget,
        "WITH incoming AS (SELECT 2 AS id, 'B' AS value UNION ALL SELECT 5, 'e') \
         MERGE INTO target_rows AS target USING incoming AS source \
         ON target.id=source.id \
         WHEN MATCHED THEN UPDATE SET value=source.value \
         WHEN NOT MATCHED THEN INSERT (id,value) VALUES (source.id,source.value)",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT id, value FROM target_rows ORDER BY id"
        )),
        ["1|a", "2|B", "3|c", "5|e"]
    );

    let response = run_with(
        &mut engine,
        &mut budget,
        "WITH supplied AS (SELECT 'six' AS value) \
         INSERT INTO target_rows VALUES (5, (SELECT value FROM supplied)) \
         ON CONFLICT (id) DO UPDATE SET value=(SELECT upper(value) FROM supplied) \
         RETURNING id, value, (SELECT count(*) FROM supplied)",
    );
    assert_eq!(
        data_rows(&response),
        ["5|SIX|1"],
        "{}",
        String::from_utf8_lossy(&response)
    );
}

#[test]
fn data_modifying_ctes_chain_into_ctes_and_main_dml() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE work_items (id int PRIMARY KEY, value text);\
         CREATE TABLE archive_items (id int PRIMARY KEY, value text);\
         CREATE TABLE final_items (id int PRIMARY KEY, value text);\
         INSERT INTO work_items VALUES (1,'a'),(2,'b'),(3,'c')",
    );

    let response = run_with(
        &mut engine,
        &mut budget,
        "WITH wanted AS (SELECT id FROM work_items WHERE id <= 2),\
              moved AS (DELETE FROM work_items USING wanted \
                        WHERE work_items.id=wanted.id RETURNING work_items.id, work_items.value),\
              shifted AS (SELECT id+100 AS id, value FROM moved)\
         INSERT INTO archive_items SELECT * FROM shifted ORDER BY id RETURNING id, value",
    );
    assert_eq!(data_rows(&response), ["101|a", "102|b"]);

    let response = run_with(
        &mut engine,
        &mut budget,
        "WITH removed AS (DELETE FROM work_items WHERE id=3 RETURNING id, value),\
              copied AS (INSERT INTO archive_items \
                         SELECT id+100, value FROM removed RETURNING id, value)\
         INSERT INTO final_items SELECT id, value FROM copied RETURNING id, value",
    );
    assert_eq!(data_rows(&response), ["103|c"]);
}

#[test]
fn recursive_cte_feeds_data_modifying_main_statement() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE generated_rows (id int PRIMARY KEY)",
    );
    let response = run_with(
        &mut engine,
        &mut budget,
        "WITH RECURSIVE numbers(n) AS (\
             VALUES (1) UNION ALL SELECT n+1 FROM numbers WHERE n < 4\
         ) INSERT INTO generated_rows SELECT n FROM numbers RETURNING id",
    );
    assert_eq!(data_rows(&response), ["1", "2", "3", "4"]);
}

#[test]
fn data_modifying_cte_preflight_and_view_targets() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE cte_view_base (id int PRIMARY KEY, value text, secret text);\
         INSERT INTO cte_view_base VALUES (1,'a','s1'),(2,'b','s2');\
         CREATE VIEW cte_view AS SELECT id, value FROM cte_view_base WHERE id <= 2",
    );

    let response = run_with(
        &mut engine,
        &mut budget,
        "WITH changed AS (\
             UPDATE cte_view SET value=upper(value) WHERE id=1 RETURNING id, value\
         ) DELETE FROM cte_view USING changed \
         WHERE cte_view.id=changed.id+1 RETURNING cte_view.id, cte_view.value",
    );
    assert_eq!(data_rows(&response), ["2|b"]);

    let duplicate = run_with(
        &mut engine,
        &mut budget,
        "WITH same AS (DELETE FROM cte_view_base RETURNING id),\
              same AS (SELECT 1) SELECT * FROM same",
    );
    assert!(String::from_utf8_lossy(&duplicate).contains("specified more than once"));
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT id, value FROM cte_view_base ORDER BY id"
        )),
        ["1|A"]
    );

    let hidden_update = run_with(
        &mut engine,
        &mut budget,
        "UPDATE cte_view SET secret='leak' WHERE id=1",
    );
    assert!(
        String::from_utf8_lossy(&hidden_update)
            .contains("column \"secret\" of relation \"cte_view\" does not exist")
    );
    let hidden_insert = run_with(
        &mut engine,
        &mut budget,
        "INSERT INTO cte_view(secret) VALUES ('leak')",
    );
    assert!(
        String::from_utf8_lossy(&hidden_insert)
            .contains("column \"secret\" of relation \"cte_view\" does not exist")
    );

    let wildcard = run_with(
        &mut engine,
        &mut budget,
        "UPDATE cte_view SET value='visible' WHERE id=1 RETURNING *",
    );
    assert_eq!(data_rows(&wildcard), ["1|visible"]);

    let aliases = run_with(
        &mut engine,
        &mut budget,
        "WITH changed(one, two) AS (\
             UPDATE cte_view_base SET value='x' RETURNING id\
         ) SELECT * FROM changed",
    );
    assert!(String::from_utf8_lossy(&aliases).contains("1 columns available but 2 columns"));
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT id, value FROM cte_view_base ORDER BY id"
        )),
        ["1|visible"]
    );
}

#[test]
fn srf_in_value_subquery() {
    // A set-returning function in the select list of an IN / ANY / ARRAY
    // subquery expands to its set of rows (matching PostgreSQL).
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE sr (id int)");
    run_with(&mut e, &mut b, "INSERT INTO sr VALUES (1),(2),(3)");
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT id FROM sr WHERE id IN (SELECT unnest(ARRAY[1,3])) ORDER BY id"
        )),
        ["1", "3"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT id FROM sr WHERE id = ANY (SELECT generate_series(2,3)) ORDER BY id"
        )),
        ["2", "3"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT id FROM sr WHERE id NOT IN (SELECT unnest(ARRAY[2])) ORDER BY id"
        )),
        ["1", "3"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT array(SELECT unnest(ARRAY[5,6]) ORDER BY 1)"
        )),
        ["{5,6}"]
    );
}

#[test]
fn in_subquery_empty_and_null_semantics() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE empt (x int)");
    run_with(&mut e, &mut b, "CREATE TABLE nn (x int)");
    run_with(&mut e, &mut b, "INSERT INTO nn VALUES (NULL)");
    // Over an empty set, IN is FALSE and NOT IN is TRUE even for NULL.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT 1 IN (SELECT * FROM empt)"
        )),
        ["f"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT NULL IN (SELECT * FROM empt)"
        )),
        ["f"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT NULL NOT IN (SELECT * FROM empt)"
        )),
        ["t"]
    );
    // A NULL operand against a non-empty set is unknown (NULL).
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT NULL IN (SELECT * FROM nn)"
        )),
        ["NULL"]
    );
    // A value absent from a set that contains NULL is unknown (NULL).
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT 1 IN (SELECT * FROM nn)")),
        ["NULL"]
    );
}

#[test]
fn in_subquery_operand_type_check() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE ti (x int)");
    // A string literal that cannot become the column type errors even over
    // an empty set, as PostgreSQL does (invalid_text_representation).
    let bytes = run_with(&mut e, &mut b, "SELECT 'hello' IN (SELECT * FROM ti)");
    assert!(
        String::from_utf8_lossy(&bytes).contains("22P02"),
        "{:?}",
        String::from_utf8_lossy(&bytes)
    );
    // A numeric string still coerces fine and is simply not present.
    run_with(&mut e, &mut b, "INSERT INTO ti VALUES (NULL)");
    let bytes = run_with(&mut e, &mut b, "SELECT 'hello' NOT IN (SELECT * FROM ti)");
    assert!(String::from_utf8_lossy(&bytes).contains("22P02"));
}

#[test]
fn subquery_wildcard_multi_column_errors() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t2 (a int, b int)");
    run_with(&mut e, &mut b, "INSERT INTO t2 VALUES (1,2)");
    // SELECT * over a two-column source is not a scalar/IN subquery.
    let bytes = run_with(&mut e, &mut b, "SELECT 1 IN (SELECT * FROM t2)");
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("42601"), "{text}");
}

#[test]
fn scalar_functions() {
    let (mut e, mut b) = test_engine();
    let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
    assert_eq!(r(&mut e, &mut b, "SELECT trim('  hi  ')"), ["hi"]);
    assert_eq!(r(&mut e, &mut b, "SELECT ltrim('xxhi', 'x')"), ["hi"]);
    assert_eq!(r(&mut e, &mut b, "SELECT rtrim('hixx', 'x')"), ["hi"]);
    assert_eq!(r(&mut e, &mut b, "SELECT substr('hello', 2, 3)"), ["ell"]);
    assert_eq!(r(&mut e, &mut b, "SELECT substr('hello', 2)"), ["ello"]);
    assert_eq!(r(&mut e, &mut b, "SELECT substr('hello', -1, 3)"), ["h"]);
    assert_eq!(
        r(&mut e, &mut b, "SELECT replace('a-b-c', '-', '+')"),
        ["a+b+c"]
    );
    assert_eq!(r(&mut e, &mut b, "SELECT repeat('ab', 3)"), ["ababab"]);
    assert_eq!(r(&mut e, &mut b, "SELECT reverse('abc')"), ["cba"]);
    assert_eq!(r(&mut e, &mut b, "SELECT left('hello', 3)"), ["hel"]);
    assert_eq!(r(&mut e, &mut b, "SELECT left('hello', -2)"), ["hel"]);
    assert_eq!(r(&mut e, &mut b, "SELECT right('hello', 3)"), ["llo"]);
    assert_eq!(r(&mut e, &mut b, "SELECT right('hello', -2)"), ["llo"]);
    assert_eq!(r(&mut e, &mut b, "SELECT strpos('hello', 'll')"), ["3"]);
    assert_eq!(r(&mut e, &mut b, "SELECT strpos('hello', 'z')"), ["0"]);
    assert_eq!(
        r(&mut e, &mut b, "SELECT concat('a', NULL, 'b', 1)"),
        ["ab1"]
    );
    assert_eq!(
        r(&mut e, &mut b, "SELECT concat_ws(',', 'a', NULL, 'b')"),
        ["a,b"]
    );
    assert_eq!(
        r(&mut e, &mut b, "SELECT initcap('hello world')"),
        ["Hello World"]
    );
    assert_eq!(r(&mut e, &mut b, "SELECT ascii('A')"), ["65"]);
    assert_eq!(r(&mut e, &mut b, "SELECT chr(65)"), ["A"]);
    assert_eq!(r(&mut e, &mut b, "SELECT octet_length('héllo')"), ["6"]);
    assert_eq!(r(&mut e, &mut b, "SELECT greatest(3, 1, 2)"), ["3"]);
    assert_eq!(r(&mut e, &mut b, "SELECT least(3, 1, 2)"), ["1"]);
    assert_eq!(r(&mut e, &mut b, "SELECT nullif(5, 5)"), ["NULL"]);
    assert_eq!(r(&mut e, &mut b, "SELECT nullif(5, 6)"), ["5"]);
}

#[test]
fn padding_and_split_functions() {
    let (mut e, mut b) = test_engine();
    let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
    assert_eq!(r(&mut e, &mut b, "SELECT lpad('hi', 5)"), ["   hi"]);
    assert_eq!(r(&mut e, &mut b, "SELECT lpad('hi', 5, 'ab')"), ["abahi"]);
    assert_eq!(r(&mut e, &mut b, "SELECT lpad('hello', 3)"), ["hel"]);
    assert_eq!(r(&mut e, &mut b, "SELECT rpad('hi', 5, '*')"), ["hi***"]);
    assert_eq!(
        r(&mut e, &mut b, "SELECT split_part('a,b,c', ',', 2)"),
        ["b"]
    );
    assert_eq!(
        r(&mut e, &mut b, "SELECT split_part('a,b,c', ',', -1)"),
        ["c"]
    );
    assert_eq!(
        r(&mut e, &mut b, "SELECT split_part('a,b,c', ',', 5)"),
        [""]
    );
    assert_eq!(
        r(&mut e, &mut b, "SELECT translate('hello', 'el', 'ip')"),
        ["hippo"]
    );
    assert_eq!(
        r(&mut e, &mut b, "SELECT translate('hello', 'l', '')"),
        ["heo"]
    );
}

#[test]
fn bool_aggregates() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (g int, flag bool)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO t VALUES (1,true),(1,true),(2,true),(2,false),(3,NULL)",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT g, bool_and(flag), bool_or(flag) FROM t GROUP BY g ORDER BY g"
        )),
        ["1|t|t", "2|f|t", "3|NULL|NULL"]
    );
    // Whole-table aggregate + `every` alias for bool_and.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT bool_or(flag), every(flag) FROM t"
        )),
        ["t|f"]
    );
}

#[test]
fn create_index_and_unique() {
    // Validated against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (a int, b int, c int)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO t VALUES (1,1,10),(1,2,20),(2,1,30)",
    );
    // A non-unique index publishes an equality-probe cache binding.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "CREATE INDEX i1 ON t(c)"))
            .contains("CREATE INDEX")
    );
    let table_slot = e.storage.find_table("public", "t").unwrap();
    assert!(
        e.storage.value_cache_complete(table_slot, &[2]),
        "committing a named index must publish its access-path binding immediately"
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT a,b,c FROM t ORDER BY a,b"
        )),
        ["1|1|10", "1|2|20", "2|1|30"]
    );
    run_with(
        &mut e,
        &mut b,
        "CREATE INDEX ordered ON t(c DESC NULLS LAST, b ASC NULLS FIRST)",
    );
    let ordered = e
        .storage
        .indexes_for("public", "t", 0)
        .find(|index| index.name.as_str() == "ordered")
        .expect("ordered index catalog row");
    assert_eq!(&ordered.columns[..2], &[2, 1]);
    assert_eq!(&ordered.descending[..2], &[true, false]);
    assert_eq!(&ordered.nulls_first[..2], &[false, true]);
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT indexdef FROM pg_indexes WHERE indexname = 'ordered'"
        )),
        ["CREATE INDEX ordered ON public.t USING btree (c DESC NULLS LAST, b NULLS FIRST)"]
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT a,b FROM t WHERE c=20")),
        ["1|2"]
    );
    // Duplicate index name errors; unknown column errors.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "CREATE INDEX i1 ON t(a)"))
            .contains("42P07")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "CREATE INDEX i2 ON t(nope)"))
            .contains("42703")
    );
    // A composite UNIQUE index over non-duplicate data succeeds and then
    // enforces the constraint on inserts.
    run_with(&mut e, &mut b, "CREATE UNIQUE INDEX u1 ON t(a,b)");
    assert!(e.storage.value_cache_complete(table_slot, &[0, 1]));
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,1,99)"))
            .contains("23505")
    );
    // A distinct (a,b) tuple is fine.
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (2,2,40)");
    // NULLs in a unique index do not conflict (SQL semantics).
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (NULL,1,1),(NULL,1,2)");
    // CREATE UNIQUE INDEX over duplicate existing rows fails.
    run_with(&mut e, &mut b, "CREATE TABLE d (x int)");
    run_with(&mut e, &mut b, "INSERT INTO d VALUES (5),(5)");
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "CREATE UNIQUE INDEX ud ON d(x)"))
            .contains("23505")
    );
    // DROP INDEX removes the constraint: the once-conflicting insert works.
    run_with(&mut e, &mut b, "DROP INDEX u1");
    assert!(!e.storage.value_cache_complete(table_slot, &[0, 1]));
    let out = String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,1,7)"))
        .to_string();
    assert!(!out.contains("23505"), "constraint should be gone: {out}");
}

#[test]
fn joins_are_not_capped_at_eight_edges() {
    let mut config = test_config("wide-join");
    config.max_tables = 16;
    let mut budget = Budget::new(1 << 27);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    for table in 0..10 {
        let output = run_with(
            &mut engine,
            &mut budget,
            &format!("CREATE TABLE j{table} (id int); INSERT INTO j{table} VALUES (1)"),
        );
        assert!(
            !String::from_utf8_lossy(&output).contains("ERROR"),
            "table {table}: {}",
            String::from_utf8_lossy(&output)
        );
    }
    let mut sql = String::from("SELECT j0.id FROM j0");
    for table in 1..10 {
        use core::fmt::Write;
        let _ = write!(sql, " JOIN j{table} ON j{table}.id = j0.id");
    }
    let output = run_with(&mut engine, &mut budget, &sql);
    assert!(
        !String::from_utf8_lossy(&output).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(data_rows(&output), ["1"]);
}

#[test]
fn uniqueness_cache_capacity_never_limits_table_correctness() {
    let mut config = test_config("value-index-cache-capacity");
    config.table_rows = 32;
    config.value_index_rows = 1;
    let mut budget = Budget::new(1 << 26);
    let mut engine = Engine::new(&config, &mut budget).unwrap();

    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE constrained (id int PRIMARY KEY, value text);\
         INSERT INTO constrained VALUES (1,'one'),(2,'two'),(3,'three')",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT id,value FROM constrained ORDER BY id"
        )),
        ["1|one", "2|two", "3|three"]
    );
    let table_slot = engine.storage.find_table("public", "constrained").unwrap();
    assert!(
        !engine.storage.value_cache_complete(table_slot, &[0]),
        "the one-entry cache must be explicitly incomplete after three keys"
    );
    let duplicate = run_with(
        &mut engine,
        &mut budget,
        "INSERT INTO constrained VALUES (2,'duplicate')",
    );
    assert!(
        String::from_utf8_lossy(&duplicate).contains("23505"),
        "an incomplete acceleration cache must fall through to authoritative rows"
    );
}

#[test]
fn failed_index_cache_reservation_restores_every_pool_slot() {
    let mut config = test_config("value-index-cache-pool");
    config.max_value_indexes = 1;
    let mut budget = Budget::new(1 << 26);
    let mut engine = Engine::new(&config, &mut budget).unwrap();

    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE indexed (a int, b int);\
         INSERT INTO indexed VALUES (1,10),(2,20);\
         CREATE INDEX indexed_a ON indexed(a)",
    );
    let table_slot = engine.storage.find_table("public", "indexed").unwrap();
    assert!(engine.storage.value_cache_complete(table_slot, &[0]));

    let exhausted = run_with(
        &mut engine,
        &mut budget,
        "CREATE INDEX indexed_b ON indexed(b)",
    );
    assert!(
        String::from_utf8_lossy(&exhausted).contains("54000"),
        "a second distinct cache binding must fail loudly"
    );
    assert!(
        engine.storage.value_cache_complete(table_slot, &[0]),
        "failed preflight must release its partial acquisition and restore the prior binding"
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT b FROM indexed WHERE a=2"
        )),
        ["20"]
    );
}

#[test]
fn updatable_view_dml() {
    // DML on an auto-updatable view rewrites to the base table (PG 18.4).
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t1 (x int, y text)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO t1 VALUES (1,'a'),(2,'b'),(3,'c'),(-1,'neg')",
    );
    run_with(
        &mut e,
        &mut b,
        "CREATE VIEW v AS SELECT x FROM t1 WHERE x>0",
    );
    run_with(&mut e, &mut b, "DELETE FROM v WHERE x=2");
    run_with(&mut e, &mut b, "UPDATE v SET x=5 WHERE x=1");
    run_with(&mut e, &mut b, "INSERT INTO v VALUES (9)");
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT x,y FROM t1 ORDER BY x")),
        ["-1|neg", "3|c", "5|a", "9|NULL"]
    );
    // Too many values for the view's exposed columns errors like PG.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO v VALUES (2,'z')"))
            .contains("42601")
    );
    // The view itself still reads correctly (base filtered).
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT x FROM v ORDER BY x")),
        ["3", "5", "9"]
    );
}

#[test]
fn where_error_safe_conjuncts_first() {
    // PostgreSQL's qual order is unspecified/cost-driven, so a filtering
    // condition can run before an error-prone one; we match by evaluating
    // error-safe conjuncts first. Validated against PG 18.4.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (x int)");
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (1),(2),(0),(3)");
    // The x=0 row is filtered by x>0 before 100/x evaluates — no error.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT x FROM t WHERE 100/x>10 AND x>0 ORDER BY x"
        )),
        ["1", "2", "3"]
    );
    // Order of the conjuncts does not matter.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT x FROM t WHERE x<>0 AND 100/x>=33 ORDER BY x"
        )),
        ["1", "2", "3"]
    );
    // With no filtering conjunct, the error still surfaces (as in PG).
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT x FROM t WHERE 100/x>10"))
            .contains("22012")
    );
}

#[test]
fn transactional_ddl_rollback() {
    // View/index DDL is rolled back with the transaction (PG semantics).
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (a int, c int)");
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,10),(2,20)");
    // CREATE VIEW rolled back → the view is gone.
    run_with(
        &mut e,
        &mut b,
        "BEGIN; CREATE VIEW v AS SELECT a FROM t; ROLLBACK",
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT * FROM v")).contains("42P01")
    );
    // CREATE VIEW committed → persists; DROP VIEW rolled back → survives.
    run_with(
        &mut e,
        &mut b,
        "BEGIN; CREATE VIEW v AS SELECT a FROM t; COMMIT",
    );
    run_with(&mut e, &mut b, "BEGIN; DROP VIEW v; ROLLBACK");
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT a FROM v ORDER BY a")),
        ["1", "2"]
    );
    // CREATE OR REPLACE rolled back → the original definition is restored.
    run_with(
        &mut e,
        &mut b,
        "BEGIN; CREATE OR REPLACE VIEW v AS SELECT a FROM t WHERE a>1; ROLLBACK",
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT a FROM v ORDER BY a")),
        ["1", "2"]
    );
    // CREATE UNIQUE INDEX rolled back → the constraint is gone.
    run_with(
        &mut e,
        &mut b,
        "BEGIN; CREATE UNIQUE INDEX u ON t(a); ROLLBACK",
    );
    let out = String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,99)"))
        .to_string();
    assert!(
        !out.contains("23505"),
        "index constraint should be gone: {out}"
    );
    // DROP TABLE rolled back → the table and its UNIQUE index both revive.
    run_with(&mut e, &mut b, "CREATE TABLE u2 (k int)");
    run_with(&mut e, &mut b, "INSERT INTO u2 VALUES (1),(2)");
    run_with(&mut e, &mut b, "CREATE UNIQUE INDEX uk ON u2(k)");
    run_with(&mut e, &mut b, "BEGIN; DROP TABLE u2; ROLLBACK");
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO u2 VALUES (1)"))
            .contains("23505")
    );
}

#[test]
fn catalog_joins_and_subqueries() {
    // Joins/subqueries across catalog relations, validated against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE demo (a int, b text)");
    // pg_class JOIN pg_attribute on oid = attrelid.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT c.relname, a.attname FROM pg_class c \
             JOIN pg_attribute a ON a.attrelid = c.oid \
             WHERE c.relname='demo' AND a.attnum > 0 ORDER BY a.attnum"
        )),
        ["demo|a", "demo|b"]
    );
    // A catalog relation inside a subquery.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT count(*) FROM pg_attribute \
             WHERE attrelid IN (SELECT oid FROM pg_class WHERE relname='demo') AND attnum>0"
        )),
        ["2"]
    );
}

#[test]
fn psql_catalog_listing_contracts() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE sized_relation (id integer);
         INSERT INTO sized_relation VALUES (1)",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT d.datname, t.spcname,
                    pg_database_size(d.datname) >= pg_table_size('sized_relation'::regclass)
             FROM pg_database d
             JOIN pg_tablespace t ON t.oid = d.dattablespace"
        )),
        ["postgres|pg_default|t"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb,
                    rolcanlogin, rolconnlimit, rolreplication, rolbypassrls
             FROM pg_roles"
        )),
        ["postgres|t|t|t|t|t|-1|t|t"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT pg_tablespace_location(1663),
                    pg_tablespace_size(1663) = pg_database_size('postgres')"
        )),
        ["|t"]
    );
    for query in [
        "SELECT count(*) FROM pg_proc p LEFT JOIN pg_language l ON l.oid=p.prolang",
        "SELECT count(*) FROM pg_publication",
        "SELECT count(*) FROM pg_foreign_server s JOIN pg_foreign_data_wrapper f ON f.oid=s.srvfdw",
        "SELECT count(*) FROM pg_db_role_setting s LEFT JOIN pg_database d ON d.oid=s.setdatabase",
        "SELECT count(*) FROM pg_parameter_acl",
        "SELECT count(*) FROM pg_collation c JOIN pg_namespace n ON n.oid=c.collnamespace",
    ] {
        let output = run_with(&mut engine, &mut budget, query);
        assert_eq!(
            data_rows(&output),
            ["0"],
            "{query}: {}",
            String::from_utf8_lossy(&output)
        );
    }
}

#[test]
fn pg_dump_bootstrap_surface() {
    let (mut engine, mut budget) = test_engine();
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT pg_catalog.pg_is_in_recovery()"
        )),
        ["f"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT oid, rolname FROM pg_catalog.pg_roles ORDER BY 1"
        )),
        ["10|postgres"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT count(*) FROM pg_extension x \
             JOIN pg_namespace n ON n.oid=x.extnamespace"
        )),
        ["0"]
    );
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut engine,
            &mut budget,
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY"
        ))
        .contains("SET")
    );
}

#[test]
fn set_transaction_changes_only_named_characteristics() {
    let (mut engine, mut budget) = test_engine();
    let mut transaction = TxnState::new(&mut budget, 256).unwrap();

    let outside = run_txn(
        &mut engine,
        &mut budget,
        &mut transaction,
        "SET TRANSACTION READ ONLY",
    );
    assert!(outside.contains("25P01"), "{outside}");
    assert!(!transaction.read_only);

    run_txn(
        &mut engine,
        &mut budget,
        &mut transaction,
        "BEGIN ISOLATION LEVEL REPEATABLE READ READ WRITE NOT DEFERRABLE",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut transaction,
        "SET TRANSACTION READ ONLY",
    );
    assert_eq!(transaction.isolation, IsolationLevel::RepeatableRead);
    assert!(transaction.read_only);
    assert!(!transaction.deferrable);
    let nested = run_txn(&mut engine, &mut budget, &mut transaction, "BEGIN");
    assert!(nested.contains("25001"), "{nested}");
    assert_eq!(transaction.isolation, IsolationLevel::RepeatableRead);
    assert!(transaction.read_only);
    assert!(!transaction.deferrable);
    run_txn(&mut engine, &mut budget, &mut transaction, "ROLLBACK");

    run_txn(&mut engine, &mut budget, &mut transaction, "BEGIN");
    run_txn(&mut engine, &mut budget, &mut transaction, "SELECT 1");
    let late_isolation = run_txn(
        &mut engine,
        &mut budget,
        &mut transaction,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    );
    assert!(late_isolation.contains("25001"), "{late_isolation}");
    run_txn(&mut engine, &mut budget, &mut transaction, "ROLLBACK");
}

#[test]
fn repeatable_read_retains_committed_row_history() {
    let (mut engine, mut budget) = test_engine();
    let mut writer = TxnState::new(&mut budget, 256).unwrap();
    let mut reader = TxnState::new(&mut budget, 256).unwrap();

    run_txn(
        &mut engine,
        &mut budget,
        &mut writer,
        "CREATE TABLE snapshot_rows (id int PRIMARY KEY, value text)",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut writer,
        "INSERT INTO snapshot_rows VALUES (1, 'before')",
    );

    let begun = run_txn(
        &mut engine,
        &mut budget,
        &mut reader,
        "BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY",
    );
    assert!(begun.contains("BEGIN"), "{begun}");
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut reader,
            "SELECT value FROM snapshot_rows WHERE id = 1",
        )),
        ["before"]
    );

    run_txn(
        &mut engine,
        &mut budget,
        &mut writer,
        "UPDATE snapshot_rows SET id = 2, value = 'after' WHERE id = 1",
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut reader,
            "SELECT value FROM snapshot_rows WHERE id = 1",
        )),
        ["before"],
        "repeatable-read snapshot must ignore a later commit"
    );
    let read_only = run_txn(
        &mut engine,
        &mut budget,
        &mut reader,
        "DELETE FROM snapshot_rows WHERE id = 1",
    );
    assert!(read_only.contains("25006"), "{read_only}");
    run_txn(&mut engine, &mut budget, &mut reader, "ROLLBACK");

    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut writer,
            "SELECT value FROM snapshot_rows WHERE id = 2",
        )),
        ["after"]
    );
}

#[test]
fn serializable_rejects_write_skew_at_commit() {
    let (mut engine, mut budget) = test_engine();
    let mut first = TxnState::new(&mut budget, 256).unwrap();
    let mut second = TxnState::new(&mut budget, 256).unwrap();
    run_txn(
        &mut engine,
        &mut budget,
        &mut first,
        "CREATE TABLE serial_doctors (id int primary key, on_call bool)",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut first,
        "INSERT INTO serial_doctors VALUES (1, true), (2, true)",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut first,
        "BEGIN ISOLATION LEVEL SERIALIZABLE",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut second,
        "BEGIN ISOLATION LEVEL SERIALIZABLE",
    );
    assert_eq!(
        first.isolation,
        IsolationLevel::Serializable,
        "SERIALIZABLE is a real transaction mode"
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut first,
            "SELECT count(*) FROM serial_doctors WHERE on_call",
        )),
        ["2"]
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut second,
            "SELECT count(*) FROM serial_doctors WHERE on_call",
        )),
        ["2"]
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut first,
        "UPDATE serial_doctors SET on_call = false WHERE id = 1",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut second,
        "UPDATE serial_doctors SET on_call = false WHERE id = 2",
    );
    let first_commit = run_txn(&mut engine, &mut budget, &mut first, "COMMIT");
    assert!(first_commit.contains("COMMIT"), "{first_commit}");
    let second_commit = run_txn(&mut engine, &mut budget, &mut second, "COMMIT");
    assert!(second_commit.contains("40001"), "{second_commit}");
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT count(*) FROM serial_doctors WHERE on_call"
        )),
        ["1"]
    );
}

#[test]
fn object_store_checkpoint_preserves_snapshot_and_survives_cold_cache() {
    use core::sync::atomic::{AtomicU32, Ordering};

    static NEXT_BUCKET: AtomicU32 = AtomicU32::new(0);
    let sequence = NEXT_BUCKET.fetch_add(1, Ordering::SeqCst);
    let mut config = test_config(&format!("object-snapshot-{sequence}"));
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("sql-object-snapshot-{}-{sequence}", std::process::id());
    config.object_store_response_bytes = 1 << 20;
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.wal_upload_buffer_bytes = 256 * 1024;
    config.block_cache_bytes = 512 * 1024;
    config.disk_cache_bytes = 1 << 20;
    config.value_index_rows = 1;
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);

    let mut budget = Budget::new(1 << 28);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut writer = TxnState::new(&mut budget, 256).unwrap();
    let mut reader = TxnState::new(&mut budget, 256).unwrap();
    run_txn(
        &mut engine,
        &mut budget,
        &mut writer,
        "CREATE TABLE snapshot_rows (id int PRIMARY KEY, value text)",
    );
    run_txn(
        &mut engine,
        &mut budget,
        &mut writer,
        "INSERT INTO snapshot_rows VALUES (1, 'before'), (2, 'remove-me'), (3, 'keep-me')",
    );
    assert!(engine.checkpoint().unwrap());
    run_txn(
        &mut engine,
        &mut budget,
        &mut reader,
        "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY",
    );
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut reader,
            "SELECT value FROM snapshot_rows ORDER BY id",
        )),
        ["before", "remove-me", "keep-me"]
    );
    // Cross both the resident history bound and the immutable-SST list bound.
    // Every checkpoint must be free to publish and compact while the old
    // snapshot remains pinned; the object store, not either cache tier, owns
    // the durable version chain.
    for version in 1..=24 {
        let updated = run_txn(
            &mut engine,
            &mut budget,
            &mut writer,
            &format!("UPDATE snapshot_rows SET value = 'after-{version}' WHERE id = 1"),
        );
        assert!(updated.contains("UPDATE 1"), "{updated}");
        if version == 8 {
            let deleted = run_txn(
                &mut engine,
                &mut budget,
                &mut writer,
                "DELETE FROM snapshot_rows WHERE id = 2",
            );
            assert!(deleted.contains("DELETE 1"), "{deleted}");
        }
        assert!(
            engine.checkpoint().unwrap(),
            "version {version} must publish while the historical snapshot is pinned"
        );
        assert_eq!(
            data_rows(&run_with_txn_bytes(
                &mut engine,
                &mut budget,
                &mut reader,
                "SELECT value FROM snapshot_rows ORDER BY id",
            )),
            ["before", "remove-me", "keep-me"],
            "version {version} must preserve the object-resident snapshot"
        );
    }
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut reader,
            "SELECT value FROM snapshot_rows ORDER BY id",
        )),
        ["before", "remove-me", "keep-me"],
        "compaction must retain both an overwritten row and a deleted row at the pinned snapshot"
    );
    run_txn(&mut engine, &mut budget, &mut reader, "ROLLBACK");
    assert_eq!(
        data_rows(&run_with_txn_bytes(
            &mut engine,
            &mut budget,
            &mut writer,
            "SELECT id, value FROM snapshot_rows ORDER BY id",
        )),
        ["1|after-24", "3|keep-me"],
        "the current snapshot must honor the tombstone and newest version"
    );
    let analyzed = run_txn(
        &mut engine,
        &mut budget,
        &mut writer,
        "ANALYZE snapshot_rows",
    );
    assert!(analyzed.contains("ANALYZE"), "{analyzed}");
    engine.checkpoint().unwrap();
    drop(engine);

    std::fs::remove_dir_all(&config.data_dir).unwrap();
    let mut restarted_budget = Budget::new(1 << 28);
    let mut restarted = Engine::new(&config, &mut restarted_budget).unwrap();
    let restarted_slot = restarted
        .storage
        .find_table("public", "snapshot_rows")
        .unwrap();
    let restarted_statistics = restarted.storage.table_statistics(restarted_slot, 0);
    assert!(restarted_statistics.valid);
    assert_eq!(restarted_statistics.rows, 2);
    assert_eq!(restarted_statistics.columns[0].distinct_values, 2);
    assert!(
        !restarted.storage.value_cache_complete(restarted_slot, &[0])
            && restarted.storage.value_probe_complete(restarted_slot, &[0]),
        "cold recovery must attach the durable generation even when the one-row RAM cache is incomplete"
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT id, value FROM snapshot_rows WHERE id = 1 ORDER BY id",
        )),
        ["1|after-24"],
        "a cold RAM-and-disk cache must recover the authoritative object-store index generation"
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT id FROM snapshot_rows WHERE id >= 1 ORDER BY id",
        )),
        ["1", "3"],
        "range predicates must consume the recovered key generation"
    );
    let duplicate = run_with(
        &mut restarted,
        &mut restarted_budget,
        "INSERT INTO snapshot_rows VALUES (1, 'duplicate')",
    );
    assert!(
        String::from_utf8_lossy(&duplicate).contains("23505"),
        "an incomplete one-entry RAM cache must enforce uniqueness through the durable index"
    );
    drop(restarted);
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
}

#[test]
fn selective_object_resident_query_prunes_durable_blocks_without_warming_during_planning() {
    use core::sync::atomic::{AtomicU32, Ordering};

    static NEXT_BUCKET: AtomicU32 = AtomicU32::new(0);
    let sequence = NEXT_BUCKET.fetch_add(1, Ordering::SeqCst);
    let mut config = test_config(&format!("object-pruning-{sequence}"));
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("sql-object-pruning-{}-{sequence}", std::process::id());
    config.object_store_response_bytes = 1 << 20;
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.wal_upload_buffer_bytes = 1 << 20;
    config.wal_buffer_bytes = 1 << 20;
    config.block_cache_bytes = crate::store::BLOCK_SIZE;
    config.disk_cache_bytes = crate::store::BLOCK_SIZE;
    config.value_index_rows = 1;
    config.work_arena_bytes = 16 << 20;
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);

    let mut budget = Budget::new(1 << 28);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE pruning_rows (id int PRIMARY KEY, payload text)",
    );
    assert!(
        !String::from_utf8_lossy(&setup).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&setup)
    );
    for start in (1..=600).step_by(100) {
        let end = start + 99;
        let inserted = run_with(
            &mut engine,
            &mut budget,
            &format!(
                "INSERT INTO pruning_rows \
                 SELECT i, repeat('x', 512) FROM generate_series({start}, {end}) AS g(i)"
            ),
        );
        assert!(
            !String::from_utf8_lossy(&inserted).contains("ERROR"),
            "rows {start}..={end}: {}",
            String::from_utf8_lossy(&inserted)
        );
    }
    let analyzed = run_with(&mut engine, &mut budget, "ANALYZE pruning_rows");
    assert!(!String::from_utf8_lossy(&analyzed).contains("ERROR"));
    let _ = engine.checkpoint().unwrap();
    drop(engine);

    std::fs::remove_dir_all(&config.data_dir).unwrap();
    let mut full_budget = Budget::new(1 << 28);
    let mut full = Engine::new(&config, &mut full_budget).unwrap();
    let before_full = full.storage.block_io_stats();
    let full_result = run_with(
        &mut full,
        &mut full_budget,
        "SELECT count(*) FROM pruning_rows",
    );
    assert_eq!(
        data_rows(&full_result),
        ["600"],
        "{}",
        String::from_utf8_lossy(&full_result)
    );
    let full_gets = full
        .storage
        .block_io_stats()
        .saturating_sub(before_full)
        .object_gets;
    assert!(full_gets > 1, "fixture must occupy several durable blocks");
    drop(full);

    std::fs::remove_dir_all(&config.data_dir).unwrap();
    let mut selective_budget = Budget::new(1 << 28);
    let mut selective = Engine::new(&config, &mut selective_budget).unwrap();
    let before_plan = selective.storage.block_io_stats();
    let plan = data_rows(&run_with(
        &mut selective,
        &mut selective_budget,
        "EXPLAIN SELECT payload FROM pruning_rows WHERE id = 573",
    ));
    assert!(
        plan.iter()
            .any(|line| line.contains("Index Scan using pruning_rows_pkey")),
        "{plan:?}"
    );
    assert_eq!(
        selective.storage.block_io_stats(),
        before_plan,
        "planning must consult only resident metadata"
    );
    let before_selective = selective.storage.block_io_stats();
    let result = data_rows(&run_with(
        &mut selective,
        &mut selective_budget,
        "SELECT length(payload) FROM pruning_rows WHERE id = 573",
    ));
    assert_eq!(result, ["512"]);
    let selective_gets = selective
        .storage
        .block_io_stats()
        .saturating_sub(before_selective)
        .object_gets;
    assert!(
        selective_gets < full_gets,
        "durable index pruning must fetch fewer objects: selective={selective_gets}, full={full_gets}"
    );
    drop(selective);
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
}

#[test]
fn external_order_and_distinct_runs_use_object_storage_after_cold_cache() {
    use core::sync::atomic::{AtomicU32, Ordering};

    static NEXT_BUCKET: AtomicU32 = AtomicU32::new(0);
    let sequence = NEXT_BUCKET.fetch_add(1, Ordering::SeqCst);
    let mut config = test_config(&format!("external-runs-{sequence}"));
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("sql-external-runs-{}-{sequence}", std::process::id());
    config.object_store_response_bytes = 1 << 20;
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.wal_upload_buffer_bytes = 1 << 20;
    config.wal_buffer_bytes = 1 << 20;
    config.wal_bytes = 16 << 20;
    config.block_cache_bytes = crate::store::BLOCK_SIZE;
    config.disk_cache_bytes = crate::store::BLOCK_SIZE;
    config.table_rows = 4096;
    config.memtable_bytes = 4 << 20;
    // The sorted projection is about 1.5 MiB. This bound proves execution
    // recycles batches instead of retaining the result in the work arena.
    config.work_arena_bytes = 512 << 10;
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);

    let mut budget = Budget::new((1 << 28) + (96 << 20));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let created = run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE external_rows (id int PRIMARY KEY, payload text)",
    );
    assert!(!String::from_utf8_lossy(&created).contains("ERROR"));
    for start in (1..=3000).step_by(50) {
        let end = start + 49;
        let inserted = run_with(
            &mut engine,
            &mut budget,
            &format!(
                "INSERT INTO external_rows \
                 SELECT i, lpad(i::text, 512, '0') \
                 FROM generate_series({start}, {end}) AS g(i)"
            ),
        );
        assert!(
            !String::from_utf8_lossy(&inserted).contains("ERROR"),
            "rows {start}..={end}: {}",
            String::from_utf8_lossy(&inserted)
        );
    }
    assert!(engine.checkpoint().unwrap());
    drop(engine);

    // Both cache tiers disappear. The table and the execution runs must use
    // the provider-neutral object tier; local files cannot be authoritative.
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    let mut restarted_budget = Budget::new((1 << 28) + (96 << 20));
    let mut restarted = Engine::new(&config, &mut restarted_budget).unwrap();
    let before = restarted.storage.block_io_stats();
    let ordered = run_with(
        &mut restarted,
        &mut restarted_budget,
        "SELECT id FROM external_rows ORDER BY payload DESC LIMIT 10",
    );
    assert_eq!(
        data_rows(&ordered),
        [
            "3000", "2999", "2998", "2997", "2996", "2995", "2994", "2993", "2992", "2991"
        ],
        "{}",
        String::from_utf8_lossy(&ordered),
    );
    let traffic = restarted.storage.block_io_stats().saturating_sub(before);
    assert!(
        traffic.object_gets > 0 && traffic.object_puts > 0,
        "cold input and external runs must cross the durable block boundary: {traffic:?}"
    );

    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT DISTINCT ON (id % 10) id % 10, id \
             FROM external_rows ORDER BY id % 10, id DESC",
        )),
        [
            "0|3000", "1|2991", "2|2992", "3|2993", "4|2994", "5|2995", "6|2996", "7|2997",
            "8|2998", "9|2999"
        ]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT DISTINCT id % 17 FROM external_rows",
        )),
        [
            "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15",
            "16"
        ]
    );
    let before_membership = restarted.storage.block_io_stats();
    let membership = run_with_arena_bytes(
        &mut restarted,
        &mut restarted_budget,
        "SELECT 1 WHERE lpad('1', 512, '0') IN \
         (SELECT payload FROM external_rows)",
        256 << 10,
    );
    assert_eq!(
        data_rows(&membership),
        ["1"],
        "a subquery list larger than the arena must remain probeable: {}",
        String::from_utf8_lossy(&membership)
    );
    let membership_traffic = restarted
        .storage
        .block_io_stats()
        .saturating_sub(before_membership);
    assert!(
        membership_traffic.object_puts > 0 && membership_traffic.object_gets > 0,
        "subquery membership must spool and probe the provider-neutral run: {membership_traffic:?}"
    );
    assert_eq!(
        data_rows(&run_with_arena_bytes(
            &mut restarted,
            &mut restarted_budget,
            "SELECT 1
             WHERE ROW(1, lpad('1', 512, '0')) IN
                   (SELECT id, payload FROM external_rows WHERE id <= 2)",
            256 << 10,
        )),
        ["1"],
        "a rewritten row-valued subquery must remain a single record in its external run"
    );
    assert_eq!(
        data_rows(&run_with_arena_bytes(
            &mut restarted,
            &mut restarted_budget,
            "SELECT 1 WHERE 3001 IN (
                 SELECT id FROM external_rows
                 UNION ALL
                 VALUES (3001)
             )",
            256 << 10,
        )),
        ["1"],
        "set-operation membership must probe the external run"
    );
    let scalar = run_with_arena_bytes(
        &mut restarted,
        &mut restarted_budget,
        "SELECT length((SELECT payload FROM external_rows WHERE id > 0 LIMIT 1))",
        256 << 10,
    );
    assert_eq!(
        data_rows(&scalar),
        ["512"],
        "LIMIT must stop an externally spooled scalar subquery before its cardinality check"
    );
    let updated = run_with_arena_bytes(
        &mut restarted,
        &mut restarted_budget,
        "UPDATE external_rows SET payload = payload \
         WHERE id = 1 AND id IN (SELECT id FROM external_rows)",
        256 << 10,
    );
    assert!(
        !String::from_utf8_lossy(&updated).contains("ERROR"),
        "DML must keep its object-run probe after releasing the immutable Storage borrow: {}",
        String::from_utf8_lossy(&updated)
    );
    let before_recursive = restarted.storage.block_io_stats();
    let recursive = run_with_arena_bytes(
        &mut restarted,
        &mut restarted_budget,
        "WITH RECURSIVE r(id, payload) AS (
             SELECT id, payload FROM external_rows
             UNION ALL
             SELECT id, payload FROM r WHERE false
         )
         SELECT count(*) FROM r",
        256 << 10,
    );
    assert_eq!(
        data_rows(&recursive),
        ["3000"],
        "the recursive all/work tables must outgrow the arena: {}",
        String::from_utf8_lossy(&recursive)
    );
    assert!(
        restarted
            .storage
            .block_io_stats()
            .saturating_sub(before_recursive)
            .object_puts
            > 0,
        "recursive work tables must be immutable object-backed runs"
    );
    let lateral = run_with_arena_bytes(
        &mut restarted,
        &mut restarted_budget,
        "SELECT 1
         FROM (VALUES (1)) AS seed(n)
         CROSS JOIN LATERAL (
             SELECT payload FROM external_rows
         ) AS expanded
         WHERE expanded.payload IS NULL",
        256 << 10,
    );
    assert!(
        data_rows(&lateral).is_empty(),
        "lateral spooling changed the empty result: {}",
        String::from_utf8_lossy(&lateral)
    );
    let lateral_function = run_with_arena_bytes(
        &mut restarted,
        &mut restarted_budget,
        "SELECT generated.n
         FROM (VALUES (5000)) AS seed(stop)
         CROSS JOIN LATERAL generate_series(1, seed.stop) AS generated(n)
         WHERE generated.n < 0",
        256 << 10,
    );
    assert!(
        data_rows(&lateral_function).is_empty(),
        "lateral SRF spooling changed the empty result: {}",
        String::from_utf8_lossy(&lateral_function)
    );
    let outer_join = run_with_arena_bytes(
        &mut restarted,
        &mut restarted_budget,
        "SELECT right_side.id
         FROM (
             SELECT id FROM external_rows WHERE id = 1
         ) AS left_side
         RIGHT JOIN external_rows AS right_side
           ON left_side.id = right_side.id
         WHERE left_side.id IS NULL AND right_side.id < 0",
        256 << 10,
    );
    assert!(
        data_rows(&outer_join).is_empty(),
        "the external RIGHT JOIN match map lost matches: {}",
        String::from_utf8_lossy(&outer_join)
    );
    let union_output = run_with(
        &mut restarted,
        &mut restarted_budget,
        "SELECT id FROM external_rows WHERE id <= 4
             UNION
             SELECT id FROM external_rows WHERE id BETWEEN 3 AND 6
             ORDER BY id DESC",
    );
    assert_eq!(
        data_rows(&union_output),
        ["6", "5", "4", "3", "2", "1"],
        "UNION must merge and deduplicate provider-neutral runs: {}",
        String::from_utf8_lossy(&union_output)
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT id FROM external_rows WHERE id <= 4
             INTERSECT ALL
             SELECT id FROM external_rows WHERE id BETWEEN 3 AND 6
             ORDER BY id",
        )),
        ["3", "4"],
        "INTERSECT ALL must merge external multisets"
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT id FROM external_rows WHERE id <= 4
             EXCEPT
             SELECT id FROM external_rows WHERE id BETWEEN 3 AND 6
             ORDER BY id",
        )),
        ["1", "2"],
        "EXCEPT must merge external multisets"
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT id FROM \
             (SELECT id, payload FROM external_rows ORDER BY payload DESC) AS materialized \
             ORDER BY id LIMIT 10",
        )),
        ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"],
        "a nested consumer must stream its immutable child run while building its own"
    );
    let created = run_with_arena_bytes(
        &mut restarted,
        &mut restarted_budget,
        "CREATE TABLE external_copy AS \
         SELECT id, payload FROM \
         (SELECT id, payload FROM external_rows ORDER BY payload DESC) AS materialized \
         ORDER BY id LIMIT 10",
        1 << 20,
    );
    assert!(
        !String::from_utf8_lossy(&created).contains("ERROR"),
        "{}",
        String::from_utf8_lossy(&created)
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT id FROM external_copy ORDER BY id",
        )),
        ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"],
        "a retaining consumer must outlive recycled evaluator scratch"
    );
    let ties = data_rows(&run_with(
        &mut restarted,
        &mut restarted_budget,
        "SELECT id / 1000 FROM external_rows \
         ORDER BY id / 1000 FETCH FIRST 1 ROW WITH TIES",
    ));
    assert_eq!(ties.len(), 999);
    assert!(ties.iter().all(|value| value == "0"));

    drop(restarted);
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
}

#[test]
fn external_set_multisets_use_the_provider_neutral_block_store() {
    use core::sync::atomic::{AtomicU32, Ordering};

    static NEXT_BUCKET: AtomicU32 = AtomicU32::new(0);
    let sequence = NEXT_BUCKET.fetch_add(1, Ordering::SeqCst);
    let mut config = test_config(&format!("external-sets-{sequence}"));
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("sql-external-sets-{}-{sequence}", std::process::id());
    config.object_store_response_bytes = 1 << 20;
    config.block_cache_bytes = crate::store::BLOCK_SIZE;
    config.disk_cache_bytes = crate::store::BLOCK_SIZE;
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);
    let mut budget = Budget::new(1 << 28);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE external_set_rows (id int);
         INSERT INTO external_set_rows VALUES (1),(2),(3),(3),(4),(5),(6)",
    );
    let before = engine.storage.block_io_stats();
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT id FROM external_set_rows WHERE id <= 4
             UNION
             SELECT id FROM external_set_rows WHERE id BETWEEN 3 AND 6
             ORDER BY id DESC",
        )),
        ["6", "5", "4", "3", "2", "1"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT id FROM external_set_rows WHERE id <= 4
             INTERSECT ALL
             SELECT id FROM external_set_rows WHERE id BETWEEN 3 AND 6
             ORDER BY id",
        )),
        ["3", "3", "4"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT id FROM external_set_rows WHERE id <= 4
             EXCEPT
             SELECT id FROM external_set_rows WHERE id BETWEEN 3 AND 6
             ORDER BY id",
        )),
        ["1", "2"]
    );
    let traffic = engine.storage.block_io_stats().saturating_sub(before);
    assert!(
        traffic.object_puts > 0 && traffic.object_gets > 0,
        "set multisets must traverse BlockStore: {traffic:?}"
    );
    drop(engine);
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
}

#[test]
fn transaction_wal_isolated_across_checkpoint_interleaving_and_cold_recovery() {
    use core::sync::atomic::{AtomicU32, Ordering};

    static NEXT_BUCKET: AtomicU32 = AtomicU32::new(0);
    let sequence = NEXT_BUCKET.fetch_add(1, Ordering::SeqCst);
    let mut config = test_config(&format!("object-transaction-wal-{sequence}"));
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!(
        "sql-object-transaction-wal-{}-{sequence}",
        std::process::id()
    );
    config.object_store_response_bytes = 1 << 20;
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.wal_upload_buffer_bytes = 256 * 1024;
    config.block_cache_bytes = 512 * 1024;
    config.disk_cache_bytes = 1 << 20;
    config.max_tables = 16;
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);

    let mut budget = Budget::new(1 << 28);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut connection_a = TxnState::new(&mut budget, 256).unwrap();
    let mut connection_b = TxnState::new(&mut budget, 256).unwrap();
    let mut guc_a = GucState::new();
    let mut guc_b = GucState::new();
    run_session_transaction(
        &mut engine,
        &mut budget,
        &mut connection_b,
        &mut guc_b,
        "CREATE TABLE checkpoint_base (id int PRIMARY KEY); \
         INSERT INTO checkpoint_base VALUES (1)",
    );
    assert!(engine.checkpoint().unwrap());

    run_session_transaction(
        &mut engine,
        &mut budget,
        &mut connection_a,
        &mut guc_a,
        "BEGIN; \
         CREATE TABLE rolled_back_catalog (id int); \
         CREATE UNIQUE INDEX rolled_back_index ON checkpoint_base(id)",
    );
    // A checkpoint may publish committed state while a transaction privately
    // stages catalog WAL. It must neither publish nor discard that stage.
    assert!(engine.checkpoint().unwrap());
    run_session_transaction(
        &mut engine,
        &mut budget,
        &mut connection_b,
        &mut guc_b,
        "CREATE TABLE committed_while_a_open (id int PRIMARY KEY); \
         INSERT INTO committed_while_a_open VALUES (2)",
    );
    run_session_transaction(
        &mut engine,
        &mut budget,
        &mut connection_a,
        &mut guc_a,
        "ROLLBACK",
    );

    run_session_transaction(
        &mut engine,
        &mut budget,
        &mut connection_a,
        &mut guc_a,
        "BEGIN; \
         CREATE TABLE late_commit (id int PRIMARY KEY); \
         INSERT INTO late_commit VALUES (3); \
         CREATE INDEX late_commit_id ON late_commit(id DESC NULLS LAST); \
         SAVEPOINT before_discard; \
         CREATE TABLE savepoint_discarded (id int); \
         ROLLBACK TO SAVEPOINT before_discard; \
         CREATE TABLE after_savepoint (id int PRIMARY KEY); \
         INSERT INTO after_savepoint VALUES (4); \
         CREATE SEQUENCE late_sequence START 10 INCREMENT 5; \
         SELECT nextval('late_sequence')",
    );
    // This checkpoint advances the object-store manifest through connection
    // B's commit while connection A's later commit remains private.
    assert!(engine.checkpoint().unwrap());
    run_session_transaction(
        &mut engine,
        &mut budget,
        &mut connection_b,
        &mut guc_b,
        "CREATE TABLE middle_commit (id int PRIMARY KEY); \
         INSERT INTO middle_commit VALUES (5)",
    );
    run_session_transaction(
        &mut engine,
        &mut budget,
        &mut connection_a,
        &mut guc_a,
        "COMMIT",
    );
    drop(engine);

    // Lose both local durability and every cache. The manifest plus uploaded
    // WAL segments in the provider-neutral object store are the authority.
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    let mut restarted_budget = Budget::new(1 << 28);
    let mut restarted = Engine::new(&config, &mut restarted_budget).unwrap();
    for name in [
        "checkpoint_base",
        "committed_while_a_open",
        "late_commit",
        "after_savepoint",
        "middle_commit",
    ] {
        assert!(
            restarted.storage.find_table("public", name).is_some(),
            "committed table {name} must survive cold object-store recovery"
        );
    }
    for name in ["rolled_back_catalog", "savepoint_discarded"] {
        assert!(
            restarted.storage.find_table("public", name).is_none(),
            "rolled-back table {name} must never reach durable recovery"
        );
    }
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT indexdef FROM pg_indexes WHERE indexname = 'late_commit_id'"
        )),
        ["CREATE INDEX late_commit_id ON public.late_commit USING btree (id DESC NULLS LAST)"]
    );
    assert!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT indexdef FROM pg_indexes WHERE indexname = 'rolled_back_index'"
        ))
        .is_empty(),
        "a rolled-back index must never reach durable recovery"
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT id FROM checkpoint_base"
        )),
        ["1"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT id FROM committed_while_a_open"
        )),
        ["2"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT id FROM late_commit"
        )),
        ["3"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT id FROM after_savepoint"
        )),
        ["4"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT id FROM middle_commit"
        )),
        ["5"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut restarted,
            &mut restarted_budget,
            "SELECT nextval('late_sequence')"
        )),
        ["15"]
    );
    drop(restarted);
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
}

#[test]
fn create_view_basic() {
    // Values validated against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (id int, v int)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40)",
    );
    run_with(
        &mut e,
        &mut b,
        "CREATE VIEW big AS SELECT id, v FROM t WHERE v > 15",
    );
    // Query the view.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT id, v FROM big ORDER BY id"
        )),
        ["2|20", "3|30", "4|40"]
    );
    // Aggregate over the view.
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT sum(v) FROM big")),
        ["90"]
    );
    // A view over a view.
    run_with(
        &mut e,
        &mut b,
        "CREATE VIEW big2 AS SELECT id FROM big WHERE v > 25",
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT id FROM big2 ORDER BY id")),
        ["3", "4"]
    );
    // Duplicate view name errors; OR REPLACE succeeds.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "CREATE VIEW big AS SELECT 1"))
            .contains("42P07")
    );
    run_with(
        &mut e,
        &mut b,
        "CREATE OR REPLACE VIEW big AS SELECT id FROM t WHERE id = 1",
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT id FROM big")),
        ["1"]
    );
    // DROP VIEW; then querying it errors.
    run_with(&mut e, &mut b, "DROP VIEW big2");
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT * FROM big2")).contains("42P01")
    );
}

#[test]
fn distinct_aggregates() {
    // Values validated against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (g int, x int)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO t VALUES (1,10),(1,10),(1,20),(2,5),(2,NULL),(3,NULL)",
    );
    // Per group: DISTINCT drops duplicate 10 in group 1; NULLs never count.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT g, count(distinct x), sum(distinct x), min(distinct x), max(distinct x) \
             FROM t GROUP BY g ORDER BY g"
        )),
        ["1|2|30|10|20", "2|1|5|5|5", "3|0|NULL|NULL|NULL"]
    );
    // Whole-table: distinct set {10,20,5}, plus non-distinct for contrast.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT count(distinct x), sum(distinct x), count(x), count(*) FROM t"
        )),
        ["3|35|4|6"]
    );
    // All-NULL input: count(DISTINCT) is 0, not NULL.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT count(distinct x) FROM t WHERE x IS NULL"
        )),
        ["0"]
    );
    // avg(DISTINCT int) -> numeric with PG's 16-digit scale.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT avg(distinct x) FROM t WHERE g = 1"
        )),
        ["15.0000000000000000"]
    );
    // Empty ordered/distinct aggregate buffers never acquire arena storage.
    // They still return PostgreSQL's NULL result without constructing a slice
    // from the sentinel pointer.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT array_agg(x ORDER BY x), array_agg(distinct x) FROM t WHERE false"
        )),
        ["NULL|NULL"]
    );
    // DISTINCT outside an aggregate is rejected loudly.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT length(distinct 'x')"))
            .contains("42883")
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT length(json_agg(repeat('x', 1000))::text) \
             FROM generate_series(1, 70)"
        )),
        ["70280"],
        "JSON aggregate rendering must not truncate at a fixed stack buffer"
    );
}

#[test]
fn more_scalar_functions() {
    // Values + types validated against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
    assert_eq!(r(&mut e, &mut b, "SELECT to_hex(255)"), ["ff"]);
    assert_eq!(r(&mut e, &mut b, "SELECT to_hex(4096)"), ["1000"]);
    assert_eq!(r(&mut e, &mut b, "SELECT to_hex(-1)"), ["ffffffff"]); // two's complement
    assert_eq!(r(&mut e, &mut b, "SELECT gcd(12, 18)"), ["6"]);
    assert_eq!(r(&mut e, &mut b, "SELECT gcd(0, 0)"), ["0"]);
    assert_eq!(r(&mut e, &mut b, "SELECT lcm(4, 6)"), ["12"]);
    assert_eq!(r(&mut e, &mut b, "SELECT lcm(0, 5)"), ["0"]);
    assert_eq!(r(&mut e, &mut b, "SELECT bit_length('abc')"), ["24"]);
    assert_eq!(
        r(&mut e, &mut b, "SELECT md5('abc')"),
        ["900150983cd24fb0d6963f7d28e17f72"]
    );
    assert_eq!(
        r(
            &mut e,
            &mut b,
            "SELECT md5('The quick brown fox jumps over the lazy dog')"
        ),
        ["9e107d9d372bb6826bd81d3542a419d6"]
    );
    assert_eq!(
        r(&mut e, &mut b, "SELECT starts_with('foobar', 'foo')"),
        ["t"]
    );
    assert_eq!(
        r(&mut e, &mut b, "SELECT starts_with('foobar', 'bar')"),
        ["f"]
    );
    assert_eq!(r(&mut e, &mut b, "SELECT cbrt(27)"), ["3"]);
    assert_eq!(r(&mut e, &mut b, "SELECT factorial(0)"), ["1"]);
    assert_eq!(r(&mut e, &mut b, "SELECT factorial(5)"), ["120"]);
    assert_eq!(
        r(&mut e, &mut b, "SELECT factorial(20)"),
        ["2432902008176640000"]
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT factorial(-1)"))
            .contains("22003")
    );
    // lcm overflow errors (22003).
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "SELECT lcm(1000000000000000000, 999999999999999999)"
        ))
        .contains("22003")
    );
}

#[test]
fn trig_and_rounding_functions() {
    // Values + types validated against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
    assert_eq!(r(&mut e, &mut b, "SELECT pi()"), ["3.141592653589793"]);
    assert_eq!(r(&mut e, &mut b, "SELECT degrees(pi())"), ["180"]);
    assert_eq!(r(&mut e, &mut b, "SELECT sin(0)"), ["0"]);
    assert_eq!(r(&mut e, &mut b, "SELECT cos(0)"), ["1"]);
    assert_eq!(r(&mut e, &mut b, "SELECT cosh(0)"), ["1"]);
    assert_eq!(r(&mut e, &mut b, "SELECT tanh(0)"), ["0"]);
    // Transcendental results differ in the last bits across platform libms
    // (as PostgreSQL's own float8 output does), so compare with tolerance.
    let approx = |e: &mut Engine, b: &mut Budget, sql: &str, want: f64| {
        let got: f64 = data_rows(&run_with(e, b, sql))[0]
            .parse()
            .expect("float output");
        assert!((got - want).abs() < 1e-12, "{sql}: got {got}, want {want}");
    };
    approx(&mut e, &mut b, "SELECT sinh(1)", 1.175_201_193_643_801_4);
    approx(&mut e, &mut b, "SELECT cot(1)", 0.642_092_615_934_330_8);
    // trunc(x, n) truncates toward zero to n decimals (numeric).
    assert_eq!(r(&mut e, &mut b, "SELECT trunc(1.2345, 2)"), ["1.23"]);
    assert_eq!(r(&mut e, &mut b, "SELECT trunc(1.9999, 2)"), ["1.99"]);
    assert_eq!(r(&mut e, &mut b, "SELECT trunc(-1.2999, 1)"), ["-1.2"]);
}

#[test]
fn ordered_and_distinct_row_sources() {
    // DISTINCT / ORDER BY / LIMIT inside a derived table or CTE must be
    // honored (top-N, dedup), not dropped. Validated against PG 18.4.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (v int)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO t VALUES (5),(3),(1),(4),(2),(3),(1)",
    );
    // ORDER BY ... LIMIT inside a derived table (top-3 smallest).
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT s.v FROM (SELECT v FROM t ORDER BY v LIMIT 3) s ORDER BY s.v"
        )),
        ["1", "1", "2"]
    );
    // DISTINCT inside a derived table.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT s.v FROM (SELECT DISTINCT v FROM t) s ORDER BY s.v"
        )),
        ["1", "2", "3", "4", "5"]
    );
    // DISTINCT + ORDER BY + LIMIT inside a CTE.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "WITH c AS (SELECT DISTINCT v FROM t ORDER BY v LIMIT 2) SELECT v FROM c ORDER BY v"
        )),
        ["1", "2"]
    );
    // A SELECT DISTINCT set-operation branch.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT DISTINCT v FROM t UNION SELECT 9 ORDER BY 1"
        )),
        ["1", "2", "3", "4", "5", "9"]
    );
}

#[test]
fn grouped_row_sources() {
    // GROUP BY / aggregates as a row source: derived tables, CTEs, set-operator
    // branches, and INSERT ... SELECT. Values validated against PG 18.4.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (g int, v int)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO t VALUES (1,10),(1,20),(2,30),(2,40),(3,50)",
    );
    // Derived table over a grouped subquery.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT s.g, s.total FROM (SELECT g, sum(v) AS total FROM t GROUP BY g) s \
             ORDER BY s.g"
        )),
        ["1|30", "2|70", "3|50"]
    );
    // CTE over a grouped query.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "WITH gs AS (SELECT g, count(*) AS c FROM t GROUP BY g) \
             SELECT g, c FROM gs ORDER BY g"
        )),
        ["1|2", "2|2", "3|1"]
    );
    // Set-operation branch with an aggregate.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT count(*) FROM t UNION SELECT 1 ORDER BY 1"
        )),
        ["1", "5"]
    );
    // INSERT ... SELECT with GROUP BY.
    run_with(&mut e, &mut b, "CREATE TABLE dst (g int, total int)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO dst SELECT g, sum(v) FROM t GROUP BY g",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT g, total FROM dst ORDER BY g"
        )),
        ["1|30", "2|70", "3|50"]
    );
}

#[test]
fn common_table_expressions() {
    // Values validated against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (id int, v int)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40)",
    );
    // Single CTE referenced in the main query.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "WITH big AS (SELECT id, v FROM t WHERE v > 15) SELECT id, v FROM big ORDER BY id"
        )),
        ["2|20", "3|30", "4|40"]
    );
    // Aggregate over a CTE.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "WITH big AS (SELECT id, v FROM t WHERE v > 15) SELECT sum(v) FROM big"
        )),
        ["90"]
    );
    // A CTE that references an earlier CTE.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "WITH a AS (SELECT id, v FROM t), b AS (SELECT id, v*2 AS w FROM a WHERE v > 20) \
             SELECT id, w FROM b ORDER BY id"
        )),
        ["3|60", "4|80"]
    );
    // A CTE referenced inside a subquery.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "WITH big AS (SELECT id FROM t WHERE v > 25) \
             SELECT count(*) FROM t WHERE id IN (SELECT id FROM big)"
        )),
        ["2"]
    );
    // A CTE joined against a physical table.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "WITH j AS (SELECT id, v FROM t WHERE v >= 30) \
             SELECT t.id, j.v FROM t JOIN j ON t.id = j.id ORDER BY t.id"
        )),
        ["3|30", "4|40"]
    );
    // WITH RECURSIVE: a non-self-referencing CTE behaves like a plain one.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "WITH RECURSIVE r AS (SELECT 1) SELECT * FROM r"
        )),
        ["1"]
    );
    // A self-referencing CTE iterates to its fixpoint.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 4) \
             SELECT * FROM c ORDER BY n"
        )),
        ["1", "2", "3", "4"]
    );
    // UNION (deduplicating) terminates a cyclic recursion.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "WITH RECURSIVE c(n) AS (SELECT 1 UNION SELECT (n % 3) + 1 FROM c) \
             SELECT * FROM c ORDER BY n"
        )),
        ["1", "2", "3"]
    );
    // The required shape is enforced loudly.
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "WITH RECURSIVE r(n) AS (SELECT n + 1 FROM r) SELECT * FROM r"
        ))
        .contains("42P19")
    );
}

#[test]
fn derived_tables() {
    // Values validated against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (id int, v int)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40)",
    );
    // Simple derived table with a WHERE inside and outside.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT s.id, s.v FROM (SELECT id, v FROM t WHERE v > 15) s ORDER BY s.id"
        )),
        ["2|20", "3|30", "4|40"]
    );
    // Aggregate over a derived table.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT sum(s.v) FROM (SELECT id, v FROM t WHERE v > 15) s"
        )),
        ["90"]
    );
    // Computed column with an alias, filtered by the outer query.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT s.id, s.doubled FROM (SELECT id, v*2 AS doubled FROM t) s \
             WHERE s.doubled > 40 ORDER BY s.id"
        )),
        ["3|60", "4|80"]
    );
    // Join a physical table against a derived table.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT a.id, b.v FROM t a JOIN (SELECT id, v FROM t WHERE v > 25) b \
             ON a.id = b.id ORDER BY a.id"
        )),
        ["3|30", "4|40"]
    );
    // A derived table must have an alias.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT * FROM (SELECT 1)"))
            .contains("42601")
    );
    // A derived table as a set-operation branch (exercises describe_leaf).
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT 1 UNION SELECT * FROM (SELECT 2) s ORDER BY 1"
        )),
        ["1", "2"]
    );
    // Derived tables also work inside EXISTS / IN / scalar subqueries.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT 1 WHERE EXISTS (SELECT 1 FROM (SELECT id FROM t WHERE v > 25) s)"
        )),
        ["1"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT id FROM t WHERE v IN (SELECT s.v FROM (SELECT v FROM t WHERE v > 25) s) \
             ORDER BY id"
        )),
        ["3", "4"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT (SELECT max(s.v) FROM (SELECT v FROM t) s)"
        )),
        ["40"]
    );
}

#[test]
fn date_arithmetic() {
    // Values validated against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
    assert_eq!(
        r(&mut e, &mut b, "SELECT date '2024-01-10' + 5"),
        ["2024-01-15"]
    );
    assert_eq!(
        r(&mut e, &mut b, "SELECT date '2024-01-10' - 5"),
        ["2024-01-05"]
    );
    assert_eq!(
        r(&mut e, &mut b, "SELECT 5 + date '2024-01-10'"),
        ["2024-01-15"]
    );
    // date - date -> integer days.
    assert_eq!(
        r(
            &mut e,
            &mut b,
            "SELECT date '2024-03-01' - date '2024-01-01'"
        ),
        ["60"]
    );
    // Crossing a month boundary and a leap day.
    assert_eq!(
        r(&mut e, &mut b, "SELECT date '2024-02-28' + 1"),
        ["2024-02-29"]
    );
    // int - date is not defined in PostgreSQL.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT 5 - date '2024-01-10'"))
            .contains("42883")
    );
}

#[test]
fn statement_timeout_cancels_long_statement() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE big (n int)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO big SELECT * FROM generate_series(1, 300)",
    );
    // A three-way cross join is ~27M iterations — far longer than 1 ms.
    // (SET and the query share one batch: the test harness makes a fresh
    // session per call, and a SET takes effect within its batch.)
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "SET statement_timeout = 1; SELECT count(*) FROM big a, big b, big c"
        ))
        .contains("57014")
    );
    // With the timeout disabled the same query shape runs normally.
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT count(*) FROM big")),
        ["300"]
    );
}

#[test]
fn string_agg_aggregate() {
    // Values validated against PostgreSQL 18.4. Without an aggregate
    // ORDER BY, PostgreSQL leaves the concatenation order unspecified; our
    // scan order is a valid such order, so the non-distinct assertions
    // check the multiset of elements rather than a fixed sequence.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE s (g int, v text)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO s VALUES (1,'b'),(1,'a'),(1,NULL),(1,'a'),(2,'z')",
    );
    // Per group: NULL skipped, duplicates kept (order unspecified).
    let rows = data_rows(&run_with(
        &mut e,
        &mut b,
        "SELECT g, string_agg(v, ',') FROM s GROUP BY g ORDER BY g",
    ));
    let g1: Vec<&str> = rows[0].strip_prefix("1|").unwrap().split(',').collect();
    let mut g1s = g1.clone();
    g1s.sort_unstable();
    assert_eq!(g1s, ["a", "a", "b"]);
    assert_eq!(rows[1], "2|z");
    // All-NULL input yields NULL, not an empty string.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT string_agg(v, ',') FROM s WHERE v IS NULL"
        )),
        ["NULL"]
    );
    // DISTINCT deduplicates and emits the values in sorted order (PG).
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT string_agg(distinct v, ',') FROM s WHERE g = 1"
        )),
        ["a,b"]
    );
    // DISTINCT + ORDER BY on the aggregated expression (values validated
    // against PostgreSQL 18.4), ascending and descending.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT string_agg(distinct v, ',' ORDER BY v) FROM s"
        )),
        ["a,b,z"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT string_agg(distinct v, ',' ORDER BY v DESC) FROM s"
        )),
        ["z,b,a"]
    );
    // DISTINCT with a different sort key errors, as PostgreSQL does.
    assert!(
        String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "SELECT string_agg(distinct v, ',' ORDER BY g) FROM s"
        ))
        .contains("42P10")
    );
}

#[test]
fn string_agg_ordered() {
    // string_agg(x, sep ORDER BY key) — values validated against PG 18.4.
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE s (g int, v text, ord int)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO s VALUES (1,'b',2),(1,'a',1),(1,'c',3),(2,'z',1)",
    );
    // ORDER BY a separate key column.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT g, string_agg(v, ',' ORDER BY ord) FROM s GROUP BY g ORDER BY g"
        )),
        ["1|a,b,c", "2|z"]
    );
    // ORDER BY the value, descending.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT g, string_agg(v, ',' ORDER BY v DESC) FROM s GROUP BY g ORDER BY g"
        )),
        ["1|c,b,a", "2|z"]
    );
}

#[test]
fn math_functions() {
    // Values + types validated against PostgreSQL 18.4.
    let (mut e, mut b) = test_engine();
    let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
    assert_eq!(r(&mut e, &mut b, "SELECT floor(5.7)"), ["5"]); // numeric
    assert_eq!(r(&mut e, &mut b, "SELECT ceil(5.2)"), ["6"]);
    assert_eq!(r(&mut e, &mut b, "SELECT trunc(5.7)"), ["5"]);
    assert_eq!(r(&mut e, &mut b, "SELECT floor(-2.5)"), ["-3"]); // toward -inf
    assert_eq!(r(&mut e, &mut b, "SELECT ceil(-2.5)"), ["-2"]);
    assert_eq!(r(&mut e, &mut b, "SELECT trunc(-2.9)"), ["-2"]);
    assert_eq!(r(&mut e, &mut b, "SELECT round(2.5)"), ["3"]); // numeric: half away from zero
    assert_eq!(r(&mut e, &mut b, "SELECT round(3.5)"), ["4"]);
    assert_eq!(r(&mut e, &mut b, "SELECT round(2.5::float8)"), ["2"]); // float: half to even
    assert_eq!(r(&mut e, &mut b, "SELECT round(3.5::float8)"), ["4"]);
    assert_eq!(r(&mut e, &mut b, "SELECT round(1.2345, 2)"), ["1.23"]);
    assert_eq!(r(&mut e, &mut b, "SELECT round(1.005, 2)"), ["1.01"]);
    assert_eq!(r(&mut e, &mut b, "SELECT floor(5)"), ["5"]); // int -> double
    assert_eq!(r(&mut e, &mut b, "SELECT sign(-3)"), ["-1"]);
    assert_eq!(r(&mut e, &mut b, "SELECT sign(0.0)"), ["0"]);
    assert_eq!(r(&mut e, &mut b, "SELECT sqrt(9)"), ["3"]);
    assert_eq!(r(&mut e, &mut b, "SELECT sqrt(2)"), ["1.4142135623730951"]);
    assert_eq!(r(&mut e, &mut b, "SELECT power(2, 10)"), ["1024"]);
    assert_eq!(r(&mut e, &mut b, "SELECT mod(7, 3)"), ["1"]);
    assert_eq!(r(&mut e, &mut b, "SELECT mod(-7, 3)"), ["-1"]);
    // Errors.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT sqrt(-1)")).contains("2201F")
    );
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT mod(1, 0)")).contains("22012")
    );
}

#[test]
fn datetime_functions() {
    // Values validated against PostgreSQL 18.4 for
    // timestamp '2024-03-15 14:30:45.5'.
    let (mut e, mut b) = test_engine();
    let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
    let ts = "timestamp '2024-03-15 14:30:45.5'";
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT extract(year from {ts})")),
        ["2024"]
    );
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT extract(month from {ts})")),
        ["3"]
    );
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT extract(day from {ts})")),
        ["15"]
    );
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT extract(hour from {ts})")),
        ["14"]
    );
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT extract(dow from {ts})")),
        ["5"]
    );
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT extract(doy from {ts})")),
        ["75"]
    );
    assert_eq!(
        r(
            &mut e,
            &mut b,
            &format!("SELECT extract(quarter from {ts})")
        ),
        ["1"]
    );
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT extract(week from {ts})")),
        ["11"]
    );
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT extract(isodow from {ts})")),
        ["5"]
    );
    // extract returns numeric (second/epoch keep 6 decimals); date_part is float.
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT extract(second from {ts})")),
        ["45.500000"]
    );
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT date_part('second', {ts})")),
        ["45.5"]
    );
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT extract(epoch from {ts})")),
        ["1710513045.500000"]
    );
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT date_part('epoch', {ts})")),
        ["1710513045.5"]
    );
    // date_trunc.
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT date_trunc('year', {ts})")),
        ["2024-01-01 00:00:00"]
    );
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT date_trunc('month', {ts})")),
        ["2024-03-01 00:00:00"]
    );
    assert_eq!(
        r(&mut e, &mut b, &format!("SELECT date_trunc('hour', {ts})")),
        ["2024-03-15 14:00:00"]
    );
    assert_eq!(
        r(
            &mut e,
            &mut b,
            &format!("SELECT date_trunc('minute', {ts})")
        ),
        ["2024-03-15 14:30:00"]
    );
}

#[test]
fn set_operations() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (a int)");
    run_with(&mut e, &mut b, "CREATE TABLE u (b int)");
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (1),(2),(3)");
    run_with(&mut e, &mut b, "INSERT INTO u VALUES (2),(3),(4)");
    // UNION deduplicates and sorts by the trailing ORDER BY.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT a FROM t UNION SELECT b FROM u ORDER BY a"
        )),
        ["1", "2", "3", "4"]
    );
    // UNION ALL keeps duplicates.
    let mut all = data_rows(&run_with(
        &mut e,
        &mut b,
        "SELECT a FROM t UNION ALL SELECT b FROM u",
    ));
    all.sort();
    assert_eq!(all, ["1", "2", "2", "3", "3", "4"]);
    // INTERSECT and EXCEPT.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT a FROM t INTERSECT SELECT b FROM u ORDER BY 1"
        )),
        ["2", "3"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT a FROM t EXCEPT SELECT b FROM u ORDER BY 1"
        )),
        ["1"]
    );
    // Literal branches, dedup, LIMIT.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT 1 UNION SELECT 2 UNION SELECT 1 ORDER BY 1"
        )),
        ["1", "2"]
    );
    // INTERSECT binds tighter than UNION: 1 UNION (2 INTERSECT 2) = {1,2}.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT 1 UNION SELECT 2 INTERSECT SELECT 2 ORDER BY 1"
        )),
        ["1", "2"]
    );
    // Numeric-tower unification (int + numeric -> numeric).
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT 1 UNION SELECT 2.5 ORDER BY 1"
        )),
        ["1", "2.5"]
    );
    // Multiset ALL variants (validated against PostgreSQL 18.4).
    run_with(&mut e, &mut b, "CREATE TABLE m1 (x int)");
    run_with(&mut e, &mut b, "CREATE TABLE m2 (y int)");
    run_with(&mut e, &mut b, "CREATE TABLE m3 (z int)");
    run_with(&mut e, &mut b, "INSERT INTO m1 VALUES (1),(1),(2)");
    run_with(&mut e, &mut b, "INSERT INTO m2 VALUES (1),(2),(2)");
    run_with(&mut e, &mut b, "INSERT INTO m3 VALUES (1)");
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT x FROM m1 INTERSECT ALL SELECT y FROM m2 ORDER BY 1"
        )),
        ["1", "2"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SELECT x FROM m1 EXCEPT ALL SELECT z FROM m3 ORDER BY 1"
        )),
        ["1", "2"]
    );
    // Parenthesized branches override precedence: (1 UNION 2) INTERSECT 2 = {2}.
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "(SELECT 1 UNION SELECT 2) INTERSECT SELECT 2 ORDER BY 1"
        )),
        ["2"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "(SELECT 1) UNION (SELECT 2) ORDER BY 1"
        )),
        ["1", "2"]
    );
}

#[test]
fn set_operation_errors() {
    let (mut e, mut b) = test_engine();
    // Column-count mismatch.
    let a = run_with(&mut e, &mut b, "SELECT 1 UNION SELECT 1, 2");
    assert!(
        String::from_utf8_lossy(&a).contains("42601"),
        "{:?}",
        String::from_utf8_lossy(&a)
    );
    // An untyped literal adopts the other branch's type, then fails to
    // coerce (22P02) — matching PostgreSQL, which resolves the unknown
    // `'x'` to integer before parsing it.
    let c = run_with(&mut e, &mut b, "SELECT 1 UNION SELECT 'x'");
    assert!(
        String::from_utf8_lossy(&c).contains("22P02"),
        "{:?}",
        String::from_utf8_lossy(&c)
    );
    // A concretely-typed incompatible column is the type-mismatch error.
    let d = run_with(&mut e, &mut b, "SELECT 1 UNION SELECT 'x'::text");
    assert!(
        String::from_utf8_lossy(&d).contains("42804"),
        "{:?}",
        String::from_utf8_lossy(&d)
    );
}

#[test]
fn timezone_offset_affects_timestamptz() {
    // Reference outputs from PostgreSQL 18.4 for
    // timestamptz '2024-01-15 14:30:00+00'.
    let (mut e, mut b) = test_engine();
    let tstz = "timestamptz '2024-01-15 14:30:00+00'";
    let go = |e: &mut Engine, b: &mut Budget, sql: String| data_rows(&run_with(e, b, &sql));
    // ISO output with fixed offsets (note PostgreSQL's inverted signs).
    assert_eq!(
        go(
            &mut e,
            &mut b,
            format!("SET timezone='Etc/GMT+5'; SELECT {tstz}")
        ),
        ["2024-01-15 09:30:00-05"]
    );
    assert_eq!(
        go(
            &mut e,
            &mut b,
            format!("SET timezone='-08:00'; SELECT {tstz}")
        ),
        ["2024-01-15 22:30:00+08"]
    );
    assert_eq!(
        go(
            &mut e,
            &mut b,
            format!("SET timezone='+05:30'; SELECT {tstz}")
        ),
        ["2024-01-15 09:00:00-05:30"]
    );
    // Non-ISO zone abbreviation: Etc zones show the offset, bare offsets show
    // nothing (a trailing space), exactly as PostgreSQL does.
    assert_eq!(
        go(
            &mut e,
            &mut b,
            format!("SET datestyle='SQL'; SET timezone='Etc/GMT+5'; SELECT {tstz}")
        ),
        ["01/15/2024 09:30:00 -05"]
    );
    assert_eq!(
        go(
            &mut e,
            &mut b,
            format!("SET datestyle='Postgres'; SET timezone='-08:00'; SELECT {tstz}")
        ),
        ["Mon Jan 15 22:30:00 2024 "]
    );
    // Named zones with DST are modeled: the winter timestamp above falls in
    // standard time, so New York is -05 (matches PostgreSQL 18.4).
    assert_eq!(
        go(
            &mut e,
            &mut b,
            format!("SET timezone='America/New_York'; SELECT {tstz}")
        ),
        ["2024-01-15 09:30:00-05"]
    );
    // A summer timestamp in the same zone shows daylight time (-04).
    let summer = "timestamptz '2024-07-15 14:30:00+00'";
    assert_eq!(
        go(
            &mut e,
            &mut b,
            format!("SET timezone='America/New_York'; SELECT {summer}")
        ),
        ["2024-07-15 10:30:00-04"]
    );
    // An unknown zone name is still rejected loudly.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SET timezone='Mars/Olympus'"))
            .contains("22023")
    );
}

#[test]
fn datestyle_affects_date_output() {
    let (mut e, mut b) = test_engine();
    // ISO is the default.
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT date '2024-01-15'")),
        ["2024-01-15"]
    );
    // A SET earlier in the batch changes a later SELECT's rendering.
    let r = run_with(
        &mut e,
        &mut b,
        "SET datestyle='SQL, DMY'; SELECT date '2024-01-15', timestamp '2024-01-15 14:30:00'",
    );
    assert_eq!(data_rows(&r), ["15/01/2024|15/01/2024 14:30:00"]);
    let r = run_with(
        &mut e,
        &mut b,
        "SET datestyle='Postgres'; SELECT timestamp '2024-01-15 14:30:00'",
    );
    assert_eq!(data_rows(&r), ["Mon Jan 15 14:30:00 2024"]);
    let r = run_with(
        &mut e,
        &mut b,
        "SET datestyle='German'; SELECT date '2024-01-15'",
    );
    assert_eq!(data_rows(&r), ["15.01.2024"]);
    // Cumulative canonical form in SHOW (German defaults to DMY).
    assert_eq!(
        data_rows(&run_with(
            &mut e,
            &mut b,
            "SET datestyle='ISO,MDY'; SET datestyle='German'; SHOW datestyle"
        )),
        ["German, DMY"]
    );
}

#[test]
fn set_and_show_session_gucs() {
    // GucState is per run_with call, so SET and SHOW share one call.
    let (mut e, mut b) = test_engine();
    // A supported value is stored and reflected by SHOW.
    let r = run_with(
        &mut e,
        &mut b,
        "SET application_name = 'myapp'; SHOW application_name",
    );
    assert_eq!(data_rows(&r), ["myapp"]);
    // client_encoding accepts UTF8 (and synonyms) and rejects others.
    assert_eq!(
        message_types(&run_with(&mut e, &mut b, "SET client_encoding = 'UTF8'")),
        [b'C']
    );
    let bad = run_with(&mut e, &mut b, "SET client_encoding = 'LATIN1'");
    assert!(
        String::from_utf8_lossy(&bad).contains("0A000"),
        "{:?}",
        String::from_utf8_lossy(&bad)
    );
    // A named IANA time zone is now accepted.
    assert_eq!(
        message_types(&run_with(
            &mut e,
            &mut b,
            "SET timezone = 'America/New_York'"
        )),
        [b'C']
    );
    // An unknown zone name is still rejected loudly.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SET timezone = 'Mars/Olympus'"))
            .contains("22023")
    );
    // DateStyle values are now honored (see datestyle_affects_date_output).
    assert_eq!(
        message_types(&run_with(&mut e, &mut b, "SET DateStyle = 'German'")),
        [b'C']
    );
    // SET TIME ZONE spelling maps to timezone; UTC is accepted.
    assert_eq!(
        message_types(&run_with(&mut e, &mut b, "SET TIME ZONE 'UTC'")),
        [b'C']
    );
    // An unknown parameter is rejected.
    assert!(
        String::from_utf8_lossy(&run_with(&mut e, &mut b, "SET no_such_guc = 1")).contains("42704")
    );
    // SHOW of a fixed server parameter still works.
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SHOW server_encoding")),
        ["UTF8"]
    );
}

#[test]
fn prepare_coerces_args_to_declared_types() {
    // The prepared-statement pool is per run_with call, so PREPARE and
    // EXECUTE must share one call (one multi-statement simple query).
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (id int)");
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (5)");
    // A text argument is coerced to the declared int type.
    let r = run_with(
        &mut e,
        &mut b,
        "PREPARE p (int) AS SELECT id FROM t WHERE id = $1; EXECUTE p('5')",
    );
    assert_eq!(data_rows(&r), ["5"]);
    // An argument that cannot become the declared type errors (not ignored).
    let bad = run_with(
        &mut e,
        &mut b,
        "PREPARE p2 (int) AS SELECT $1; EXECUTE p2('nope')",
    );
    assert!(
        String::from_utf8_lossy(&bad).contains("22P02"),
        "{:?}",
        String::from_utf8_lossy(&bad)
    );
    // Wrong argument count is rejected.
    let count = run_with(
        &mut e,
        &mut b,
        "PREPARE p3 (int) AS SELECT $1; EXECUTE p3(1, 2)",
    );
    assert!(
        String::from_utf8_lossy(&count).contains("08P01"),
        "{:?}",
        String::from_utf8_lossy(&count)
    );
    // An unknown declared type is rejected at PREPARE.
    let unk = run_with(&mut e, &mut b, "PREPARE q (nosuchtype) AS SELECT $1");
    assert!(String::from_utf8_lossy(&unk).contains("42704"));
}

#[test]
fn varchar_length_is_enforced() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (s varchar(3))");
    assert_eq!(
        message_types(&run_with(&mut e, &mut b, "INSERT INTO t VALUES ('abc')")),
        [b'C']
    );
    let over = run_with(&mut e, &mut b, "INSERT INTO t VALUES ('abcd')");
    assert!(
        String::from_utf8_lossy(&over).contains("22001"),
        "{:?}",
        String::from_utf8_lossy(&over)
    );
    // The stored value is unchanged (not truncated).
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT s FROM t")),
        ["abc"]
    );
}

#[test]
fn numeric_scale_and_precision_enforced() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (n numeric(5,2))");
    // Rounds to scale 2 (half away from zero) and pads to 2 fractional digits.
    run_with(&mut e, &mut b, "INSERT INTO t VALUES (12.345), (12.5), (1)");
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT n FROM t ORDER BY n")),
        ["1.00", "12.35", "12.50"]
    );
    // Too many integer digits (p - s = 3) overflows.
    let over = run_with(&mut e, &mut b, "INSERT INTO t VALUES (1234.5)");
    assert!(
        String::from_utf8_lossy(&over).contains("22003"),
        "{:?}",
        String::from_utf8_lossy(&over)
    );
    // Rounding that carries into a new integer digit also overflows.
    let carry = run_with(&mut e, &mut b, "INSERT INTO t VALUES (999.999)");
    assert!(String::from_utf8_lossy(&carry).contains("22003"));
}

#[test]
fn type_modifier_on_wrong_type_is_rejected() {
    let (mut e, mut b) = test_engine();
    // A modifier on a type that does not take one errors loudly, in both a
    // column definition and a cast — rejected, not accepted.
    let bad = run_with(&mut e, &mut b, "CREATE TABLE t (x int(4))");
    assert!(
        String::from_utf8_lossy(&bad).contains("42601"),
        "{:?}",
        String::from_utf8_lossy(&bad)
    );
    let bad2 = run_with(&mut e, &mut b, "SELECT 1::int(4)");
    assert!(
        String::from_utf8_lossy(&bad2).contains("42601"),
        "{:?}",
        String::from_utf8_lossy(&bad2)
    );
}

#[test]
fn insert_select() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE src (a int, b text)");
    run_with(&mut e, &mut b, "CREATE TABLE dst (a int, b text)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO src VALUES (1,'x'),(2,'y'),(3,'z')",
    );
    // INSERT ... SELECT with a WHERE filter and projection.
    let bytes = run_with(
        &mut e,
        &mut b,
        "INSERT INTO dst SELECT a, b FROM src WHERE a >= 2",
    );
    assert_eq!(message_types(&bytes), [b'C']);
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT a, b FROM dst ORDER BY a")),
        ["2|y", "3|z"]
    );
    // SELECT * source.
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO dst SELECT * FROM src WHERE a = 1",
    );
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT a FROM dst ORDER BY a")),
        ["1", "2", "3"]
    );
    // Column list + constant projection; RETURNING.
    let bytes = run_with(
        &mut e,
        &mut b,
        "INSERT INTO dst (a) SELECT a * 10 FROM src WHERE a = 3 RETURNING a",
    );
    assert_eq!(data_rows(&bytes), ["30"]);
    // Self-insert reads the pre-insert snapshot (must not loop).
    run_with(&mut e, &mut b, "CREATE TABLE s2 (v int)");
    run_with(&mut e, &mut b, "INSERT INTO s2 VALUES (1),(2)");
    run_with(&mut e, &mut b, "INSERT INTO s2 SELECT v FROM s2");
    assert_eq!(
        data_rows(&run_with(&mut e, &mut b, "SELECT v FROM s2 ORDER BY v")),
        ["1", "1", "2", "2"]
    );
}

#[test]
fn insert_select_column_count_mismatch() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE src (a int, b int)");
    run_with(&mut e, &mut b, "CREATE TABLE dst (a int)");
    run_with(&mut e, &mut b, "INSERT INTO src VALUES (1,2)");
    let bytes = run_with(&mut e, &mut b, "INSERT INTO dst SELECT * FROM src");
    assert!(String::from_utf8_lossy(&bytes).contains("42601"));
}

#[test]
fn correlated_in_subquery() {
    let (mut e, mut b) = test_engine();
    run_with(&mut e, &mut b, "CREATE TABLE t (a int, g int)");
    run_with(&mut e, &mut b, "CREATE TABLE u (v int, g int)");
    run_with(
        &mut e,
        &mut b,
        "INSERT INTO t VALUES (1,100),(2,100),(3,200)",
    );
    run_with(&mut e, &mut b, "INSERT INTO u VALUES (1,100),(3,200)");
    // a IN (values of u.v sharing t's group g)
    let bytes = run_with(
        &mut e,
        &mut b,
        "SELECT a FROM t WHERE a IN (SELECT v FROM u WHERE u.g = t.g) ORDER BY a",
    );
    assert_eq!(data_rows(&bytes), ["1", "3"]);
}

#[test]
fn copy_formats_and_unsupported() {
    // The engine speaks COPY's text, CSV and binary formats. CSV-only options
    // misused in text mode, and binary of a type whose binary codec is not yet
    // emitted, refuse loudly rather than mis-read or corrupt a stream.
    let (mut engine, mut budget) = test_engine();
    let ok = run_with(&mut engine, &mut budget, "CREATE TABLE c (a int, b text)");
    assert!(!message_types(&ok).contains(&b'E'));
    run_with(&mut engine, &mut budget, "INSERT INTO c VALUES (1, 'x')");
    for statement in [
        "COPY c TO STDOUT (FORMAT csv)",
        "COPY c TO STDOUT (FORMAT csv, DELIMITER ';', NULL 'x', HEADER, QUOTE '#')",
        "COPY c TO STDOUT (FORMAT text)",
        "COPY c TO STDOUT CSV HEADER",
    ] {
        let out = run_with(&mut engine, &mut budget, statement);
        assert!(
            !message_types(&out).contains(&b'E'),
            "{statement}: {}",
            String::from_utf8_lossy(&out)
        );
    }
    // Binary of a scalar table now succeeds and emits the PGCOPY signature.
    let out = run_with(&mut engine, &mut budget, "COPY c TO STDOUT (FORMAT binary)");
    assert!(
        !message_types(&out).contains(&b'E'),
        "{:?}",
        String::from_utf8_lossy(&out)
    );
    assert!(
        out.windows(6).any(|w| w == b"PGCOPY"),
        "binary output should carry the signature"
    );
    // Binary of an array column now succeeds and carries the signature.
    run_with(&mut engine, &mut budget, "CREATE TABLE arr (a int[])");
    let out = run_with(
        &mut engine,
        &mut budget,
        "COPY arr TO STDOUT (FORMAT binary)",
    );
    assert!(
        !message_types(&out).contains(&b'E'),
        "{:?}",
        String::from_utf8_lossy(&out)
    );
    assert!(
        out.windows(6).any(|w| w == b"PGCOPY"),
        "binary array output should carry the signature"
    );
    // A CSV-only option in text mode, and HEADER in binary mode, still refuse.
    for statement in [
        "COPY c TO STDOUT (FORMAT text, QUOTE '#')",
        "COPY c TO STDOUT (FORMAT binary, HEADER)",
    ] {
        let out = run_with(&mut engine, &mut budget, statement);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("0A000"), "{statement}: {text}");
    }
    // COPY FROM STDIN in a multi-statement string has nowhere to stream.
    let out = run_with(&mut engine, &mut budget, "COPY c FROM STDIN; SELECT 1");
    assert!(String::from_utf8_lossy(&out).contains("0A000"));
}

#[test]
fn copy_from_applies_expression_defaults_sequences_and_generated_columns() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TYPE copy_mood AS ENUM ('ready', 'done');
         CREATE SEQUENCE copy_sequence START 10;
         CREATE TABLE copy_defaults (
             input integer,
             sequence_value bigint DEFAULT nextval('copy_sequence'),
             numbers integer[] DEFAULT ARRAY[1, 2],
             doubled integer GENERATED ALWAYS AS (input * 2) STORED,
             state copy_mood DEFAULT 'ready'::copy_mood
         )",
    );

    let mut send = crate::mem::FixedBuf::new(&mut budget, "copy send", 1 << 18).unwrap();
    let mut arena = Arena::new(&mut budget, "copy sql", 1 << 18).unwrap();
    let mut txn = TxnState::new(&mut budget, 1024).unwrap();
    let mut pool = test_pool(&mut budget);
    let mut cursors = test_cursors(&mut budget);
    let mut guc = GucState::new();
    {
        let mut responder = Responder::new(&mut send);
        engine
            .execute_simple(
                "COPY copy_defaults (input) FROM STDIN",
                &arena,
                &mut txn,
                &mut pool,
                &mut cursors,
                &mut guc,
                &mut responder,
                1,
            )
            .unwrap();
    }
    let setup = engine
        .take_pending_copy()
        .expect("COPY enters streaming mode");
    arena.reset();
    engine
        .copy_row_line(&setup, &mut txn, guc.seq_session(), &arena, b"5")
        .unwrap();
    engine.copy_finish(&mut txn, &guc).unwrap();

    let rows = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "SELECT input, sequence_value, numbers, doubled, state FROM copy_defaults",
    ));
    assert_eq!(rows, ["5|10|{1,2}|10|ready"]);

    let rejected = run_with(&mut engine, &mut budget, "COPY copy_defaults FROM STDIN");
    assert!(
        String::from_utf8_lossy(&rejected).contains("428C9"),
        "COPY targeting a generated column must fail at setup"
    );
}

#[test]
fn copy_query_to_stdout() {
    // COPY (query) TO STDOUT streams a query's rows in COPY's formats.
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE cq (id int, v int, s text)",
    );
    run_with(
        &mut engine,
        &mut budget,
        "INSERT INTO cq VALUES (1,10,'a'),(2,20,'b,x')",
    );

    // Default text format: tab-delimited rows for the projected columns.
    let out = run_with(
        &mut engine,
        &mut budget,
        "COPY (SELECT id, s FROM cq ORDER BY id) TO STDOUT",
    );
    let text = String::from_utf8_lossy(&out);
    assert!(!message_types(&out).contains(&b'E'), "{text}");
    assert!(
        text.contains("1\ta") && text.contains("2\tb,x"),
        "text rows: {text}"
    );
    assert!(text.contains("COPY 2"), "command tag: {text}");

    // CSV with a header quotes the embedded comma.
    let out = run_with(
        &mut engine,
        &mut budget,
        "COPY (SELECT id, s FROM cq ORDER BY id) TO STDOUT WITH CSV HEADER",
    );
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("id,s") && text.contains("2,\"b,x\""),
        "csv: {text}"
    );

    // An aggregate query streams its single row.
    let out = run_with(
        &mut engine,
        &mut budget,
        "COPY (SELECT count(*) FROM cq) TO STDOUT",
    );
    assert!(String::from_utf8_lossy(&out).contains('2'), "aggregate");

    // A query source is TO-only; FROM STDIN is rejected.
    let out = run_with(
        &mut engine,
        &mut budget,
        "COPY (SELECT id FROM cq) FROM STDIN",
    );
    assert!(
        message_types(&out).contains(&b'E'),
        "COPY (query) FROM STDIN must error"
    );
}

#[test]
fn listen_notify_engine_semantics() {
    let (mut engine, mut budget) = test_engine();

    // Connection 1 listens on two channels; the registrations take effect at
    // the (implicit) commit of each statement.
    let out = run_as(&mut engine, &mut budget, 1, "LISTEN a");
    assert!(String::from_utf8_lossy(&out).contains("LISTEN"));
    run_as(&mut engine, &mut budget, 1, "LISTEN b");
    assert!(engine.is_listening(1, "a"));
    assert!(engine.is_listening(1, "b"));
    assert!(!engine.is_listening(2, "a"));

    // Connection 2 raises a notification; after its commit the outbox carries
    // it, stamped with connection 2's PID. A channel nobody listens on still
    // enqueues (delivery filters by listener, not the raise).
    run_as(&mut engine, &mut budget, 2, "NOTIFY a, 'hi'");
    assert_eq!(engine.notifications().len(), 1);
    let n = &engine.notifications()[0];
    assert_eq!(n.pid, 2);
    assert_eq!(n.channel.as_str(), "a");
    assert_eq!(n.payload.as_str(), "hi");
    engine.clear_notifications();

    // A rolled-back NOTIFY is discarded; a committed one fires; a duplicate
    // (channel, payload) within one transaction collapses to a single entry.
    run_as(
        &mut engine,
        &mut budget,
        2,
        "BEGIN; NOTIFY b, 'x'; ROLLBACK",
    );
    assert_eq!(engine.notifications().len(), 0);
    run_as(
        &mut engine,
        &mut budget,
        2,
        "BEGIN; NOTIFY b, 'y'; NOTIFY b, 'y'; NOTIFY b, 'z'; COMMIT",
    );
    assert_eq!(engine.notifications().len(), 2);
    engine.clear_notifications();

    // UNLISTEN drops one channel; UNLISTEN * drops the rest.
    run_as(&mut engine, &mut budget, 1, "UNLISTEN a");
    assert!(!engine.is_listening(1, "a"));
    assert!(engine.is_listening(1, "b"));
    run_as(&mut engine, &mut budget, 1, "UNLISTEN *");
    assert!(!engine.is_listening(1, "b"));

    // A rolled-back LISTEN never registers.
    run_as(&mut engine, &mut budget, 3, "BEGIN; LISTEN c; ROLLBACK");
    assert!(!engine.is_listening(3, "c"));
}

#[test]
fn create_table_as_builds_and_populates() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE src (id int, name text)",
    );
    run_with(
        &mut engine,
        &mut budget,
        "INSERT INTO src VALUES (1,'a'),(2,'b'),(3,'c')",
    );

    // Basic CTAS: the command tag is SELECT <count>, and the rows land.
    let out = run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE t AS SELECT id, name FROM src WHERE id > 1",
    );
    assert!(
        String::from_utf8_lossy(&out).contains("SELECT 2"),
        "{:?}",
        String::from_utf8_lossy(&out)
    );
    let out = run_with(&mut engine, &mut budget, "SELECT id FROM t ORDER BY id");
    let s = String::from_utf8_lossy(&out);
    assert!(!s.contains("ERROR") && s.contains('2') && s.contains('3'));

    // WITH NO DATA creates the table empty.
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE e AS SELECT id FROM src WITH NO DATA",
    );
    let out = run_with(&mut engine, &mut budget, "SELECT count(*) FROM e");
    assert!(String::from_utf8_lossy(&out).contains('0'));

    // A column-name list renames the query's output columns.
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE r (x, y) AS SELECT id, name FROM src",
    );
    let out = run_with(&mut engine, &mut budget, "SELECT x, y FROM r ORDER BY x");
    assert!(
        !String::from_utf8_lossy(&out).contains("ERROR"),
        "{:?}",
        String::from_utf8_lossy(&out)
    );

    // IF NOT EXISTS skips the second create, keeping the first table's data.
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE IF NOT EXISTS n AS SELECT 1 AS v",
    );
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE IF NOT EXISTS n AS SELECT 2 AS v",
    );
    let out = run_with(&mut engine, &mut budget, "SELECT v FROM n");
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains('1') && !s.contains('2'), "{s:?}");

    // The created table is ordinary: a later INSERT works and enforces types.
    run_with(&mut engine, &mut budget, "INSERT INTO t VALUES (5, 'e')");
    let out = run_with(&mut engine, &mut budget, "SELECT count(*) FROM t");
    assert!(String::from_utf8_lossy(&out).contains('3'));
    let out = run_with(
        &mut engine,
        &mut budget,
        "INSERT INTO t VALUES ('bad', 'x')",
    );
    assert!(String::from_utf8_lossy(&out).contains("22P02"));

    // User-defined enum identity survives both CTAS and materialized-view
    // materialization, including the enum's automatically-created array type.
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TYPE ctas_mood AS ENUM ('low','high');\
         CREATE TABLE typed_src (m ctas_mood, ms ctas_mood[]);\
         INSERT INTO typed_src VALUES ('high', ARRAY['low','high']::ctas_mood[])",
    );
    let out = run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE typed_copy AS SELECT m, ms FROM typed_src",
    );
    assert!(
        String::from_utf8_lossy(&out).contains("SELECT 1"),
        "{}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT pg_typeof(m), pg_typeof(ms), m, ms FROM typed_copy"
        )),
        ["ctas_mood|ctas_mood[]|high|{low,high}"]
    );
    let out = run_with(
        &mut engine,
        &mut budget,
        "CREATE MATERIALIZED VIEW typed_materialized AS SELECT m, ms FROM typed_src",
    );
    assert!(
        String::from_utf8_lossy(&out).contains("SELECT 1"),
        "{}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT pg_typeof(m), pg_typeof(ms) FROM typed_materialized"
        )),
        ["ctas_mood|ctas_mood[]"]
    );
}

#[test]
fn count_distinct_over_extended_types() {
    // count(DISTINCT x) buffers its argument values, so it must never ride the
    // row-recycling scan: a recycled row would reclaim the buffered datums.
    // Values validated against PostgreSQL 18.4.
    let (mut engine, mut budget) = test_engine();
    let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
    // Interval equality is canonical: '1 hour' = '60 min'.
    assert_eq!(
        r(
            &mut engine,
            &mut budget,
            "SELECT count(DISTINCT x) FROM (VALUES (INTERVAL '1 hour'),(INTERVAL '60 min'),(INTERVAL '2 hours')) t(x)"
        ),
        ["2"]
    );
    // now() is stable within a statement.
    assert_eq!(
        r(
            &mut engine,
            &mut budget,
            "SELECT count(DISTINCT now()) FROM generate_series(1, 200) g"
        ),
        ["1"]
    );
    // bpchar compares blank-stripped, so char(2) 'a' = char(3) 'a '.
    assert_eq!(
        r(
            &mut engine,
            &mut budget,
            "SELECT count(DISTINCT c) FROM (VALUES ('a'::char(2)), ('a'::char(3))) t(c)"
        ),
        ["1"]
    );
    assert_eq!(
        r(
            &mut engine,
            &mut budget,
            "SELECT count(DISTINCT r) FROM (VALUES (int4range(1,2)),(int4range(2,3))) t(r)"
        ),
        ["2"]
    );
    assert_eq!(
        r(
            &mut engine,
            &mut budget,
            "SELECT count(DISTINCT a) FROM (VALUES ('1.2.3.4'::inet),('1.2.3.4'::inet)) t(a)"
        ),
        ["1"]
    );
}

#[test]
fn external_in_subquery_preserves_wildcard_column_coercion() {
    // PostgreSQL type-checks the IN operand against the subquery's column type
    // even over an empty set, so 'hello' against an integer column is 22P02.
    // The externally spooled run must carry that witness just as the inline
    // value list does.
    use core::sync::atomic::{AtomicU32, Ordering};

    static NEXT_BUCKET: AtomicU32 = AtomicU32::new(0);
    let sequence = NEXT_BUCKET.fetch_add(1, Ordering::SeqCst);
    let mut config = test_config(&format!("external-in-witness-{sequence}"));
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("sql-external-in-witness-{}-{sequence}", std::process::id());
    config.object_store_response_bytes = 1 << 20;
    config.block_cache_bytes = crate::store::BLOCK_SIZE;
    config.disk_cache_bytes = crate::store::BLOCK_SIZE;
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);
    let mut budget = Budget::new(1 << 28);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE in_witness (x INTEGER)",
    );
    for statement in [
        "SELECT 'hello' IN (SELECT * FROM in_witness)",
        "SELECT 'hello' NOT IN (SELECT * FROM in_witness)",
    ] {
        let out = run_with(&mut engine, &mut budget, statement);
        assert!(
            String::from_utf8_lossy(&out).contains("22P02"),
            "{statement}: {}",
            String::from_utf8_lossy(&out)
        );
    }
    // A conforming operand still probes the empty set as FALSE/TRUE.
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT 1 IN (SELECT * FROM in_witness), 1 NOT IN (SELECT * FROM in_witness)"
        )),
        ["f|t"]
    );
    run_with(&mut engine, &mut budget, "INSERT INTO in_witness VALUES (1)");
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT 1 IN (SELECT * FROM in_witness)"
        )),
        ["t"]
    );
    // A row-valued probe keeps its records structural through the external
    // sort that ORDER BY / LIMIT forces: rendered text would not compare.
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE row_witness (a int, c int);
         INSERT INTO row_witness VALUES (1, 10), (1, 20), (4, 40)",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT (1,1) IN (SELECT a,a FROM row_witness ORDER BY a LIMIT 1),
                    (4,4) IN (SELECT a,a FROM row_witness ORDER BY a DESC LIMIT 1),
                    (2,2) IN (SELECT a,a FROM row_witness ORDER BY a LIMIT 1)"
        )),
        ["t|t|f"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT pg_typeof(row(a,a)) FROM row_witness ORDER BY a LIMIT 1"
        )),
        ["record"]
    );
    // A correlated probe resolves outer columns in the subquery's select list
    // and ORDER BY keys, not only in its WHERE clause.
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE corr_outer (x int);
         INSERT INTO corr_outer VALUES (1),(5),(9)",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT x FROM corr_outer
             WHERE x = ANY (SELECT a + corr_outer.x - corr_outer.x FROM row_witness)
             ORDER BY x"
        )),
        ["1"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT x FROM corr_outer
             WHERE x IN (SELECT a FROM row_witness ORDER BY a + 0 * corr_outer.x LIMIT 2)
             ORDER BY x"
        )),
        ["1"]
    );
    // ARRAY(subquery) and set-subquery ORDER BY/LIMIT spool through the
    // provider-neutral run stack under spill.
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT ARRAY(SELECT a FROM row_witness ORDER BY a DESC)"
        )),
        ["{4,1,1}"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT ARRAY(SELECT a FROM row_witness UNION SELECT 9)"
        )),
        ["{1,4,9}"]
    );
    // Grouped aggregate group-key sort runs through an external sort.
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE grp_src (g int, v int);
         INSERT INTO grp_src SELECT i % 10, i FROM generate_series(1, 100) t(i)",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT g, count(*), sum(v) FROM grp_src GROUP BY g ORDER BY g"
        )),
        [
            "0|10|550", "1|10|460", "2|10|470", "3|10|480", "4|10|490",
            "5|10|500", "6|10|510", "7|10|520", "8|10|530", "9|10|540"
        ]
    );
    crate::object_store::sim::drop_bucket(&config.object_store_bucket);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
}
