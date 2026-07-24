//! Configuration: every runtime limit in one place.
//!
//! The config is the single source of truth for sizing — the memory plan
//! (and therefore the startup budget) is a pure function of it. Parsing is
//! strict: unknown keys, duplicate keys, and malformed values are errors
//! that name the offending line, never quietly ignored or defaulted.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Address the PostgreSQL wire listener binds to.
    pub listen_addr: String,
    /// Directory for the journal and the local object cache.
    pub data_dir: String,
    /// Fixed number of client connection slots.
    pub max_connections: u32,
    /// Authentication: trust | password | scram-sha-256.
    pub auth: String,
    /// This node's replica id within the cluster (0-based). Single-node
    /// deployments leave the cluster empty and this is 0.
    pub replica_id: u32,
    /// Peer addresses `host:port` for VSR, one per replica, in id order.
    /// Empty means a standalone single node (a cluster of one).
    pub cluster: Vec<String>,
    /// Shared password for password/scram auth (all users).
    pub password: String,
    /// Per-connection receive buffer (wire protocol messages are bounded
    /// by this).
    pub conn_recv_buffer_bytes: usize,
    /// Per-connection send buffer.
    pub conn_send_buffer_bytes: usize,
    /// Per-connection arena for parsing/planning one statement; reset after
    /// every statement.
    pub sql_arena_bytes: usize,
    /// Shared execution arena for materializing a single query's rows
    /// (ORDER BY / DISTINCT / GROUP BY buffers). Single-threaded execution
    /// means one instance serves every connection; reset after each
    /// statement. This is pos3ql's analogue of PostgreSQL's `work_mem`: a
    /// sort or hash aggregate that exceeds it errors (54000) rather than
    /// spilling to temporary files.
    pub work_arena_bytes: usize,
    /// Prepared-statement slots per connection (extended protocol).
    pub max_prepared: usize,
    /// Stored query text per prepared statement.
    pub prepared_bytes: usize,
    /// Portal slots per connection.
    pub max_portals: usize,
    /// Rows one transaction may touch (per connection undo capacity).
    pub txn_rows: usize,
    /// Bound-parameter bytes per portal.
    pub portal_bytes: usize,
    /// Buffered result bytes per portal (Execute max_rows paging).
    pub portal_result_bytes: usize,
    /// In-memory write buffer of the LSM before flush to object storage.
    pub memtable_bytes: usize,
    /// Preallocated size of the WAL journal file (disk).
    pub wal_bytes: usize,
    /// In-memory staging buffer for WAL records (bounds the largest record).
    pub wal_buffer_bytes: usize,
    /// Fixed number of table slots.
    pub max_tables: usize,
    /// Open SQL cursors per connection (DECLARE ... CURSOR).
    pub max_cursors: usize,
    /// Bytes of materialized rows one cursor may hold.
    pub cursor_bytes: usize,
    /// Per-table rowid-map capacity (rows resident in the memtable).
    pub table_rows: usize,
    /// RAM cache of object-storage blocks.
    pub block_cache_bytes: usize,
    /// Disk budget for locally cached objects (not RAM).
    pub disk_cache_bytes: usize,
    /// Object storage on/offset. When on, checkpoints snapshot to the bucket
    /// and a wiped node cold-starts from it; credentials are required.
    pub s3_on: bool,
    /// Upload committed WAL batches to the bucket (backup / RPO). Requires
    /// s3 = on. By default the upload is asynchronous: a commit is durable on
    /// local disk (fsync) immediately and the S3 upload is drained offset the
    /// commit path, so commit latency never includes the S3 round-trip.
    pub wal_upload: bool,
    /// Make WAL upload synchronous: a commit blocks until its batch is in the
    /// bucket (RPO=0 against total local-disk loss), at the cost of S3 latency
    /// on every commit. Off by default (asynchronous). Requires wal_upload.
    pub wal_upload_sync: bool,
    /// Accumulation buffer for asynchronous WAL upload. Commits batch into it
    /// between drains; when it fills, a commit drains synchronously
    /// (backpressure). Must exceed wal_buffer_bytes.
    pub wal_upload_buffer_bytes: usize,
    /// S3-compatible endpoint, `host:port`, plaintext HTTP (MinIO-style).
    pub s3_endpoint: String,
    pub s3_bucket: String,
    /// Prepended to every object key; lets databases share a bucket.
    pub s3_prefix: String,
    pub s3_region: String,
    /// Empty means "read AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY from the
    /// environment at startup".
    pub s3_access_key: String,
    pub s3_secret_key: String,
    /// Request/response head assembly buffer.
    pub s3_head_bytes: usize,
    /// Largest response body (bounds ranged GETs and LIST pages).
    pub s3_response_bytes: usize,
}

impl Config {
    /// Development defaults, used when no config file is given.
    pub fn default_dev() -> Self {
        Self {
            listen_addr: "127.0.0.1:5433".to_string(),
            data_dir: "./data".to_string(),
            max_connections: 64,
            auth: "trust".to_string(),
            password: String::new(),
            replica_id: 0,
            cluster: Vec::new(),
            conn_recv_buffer_bytes: 64 * KIB,
            conn_send_buffer_bytes: 64 * KIB,
            sql_arena_bytes: 256 * KIB,
            work_arena_bytes: 64 * MIB,
            max_prepared: 8,
            prepared_bytes: 8 * KIB,
            max_portals: 4,
            portal_bytes: 4 * KIB,
            portal_result_bytes: 64 * KIB,
            txn_rows: 8192,
            memtable_bytes: 64 * MIB,
            wal_bytes: 256 * MIB,
            wal_buffer_bytes: MIB,
            max_tables: 32,
            max_cursors: 4,
            cursor_bytes: 256 * 1024,
            table_rows: 8192,
            block_cache_bytes: 128 * MIB,
            disk_cache_bytes: GIB,
            s3_on: false,
            wal_upload: false,
            wal_upload_sync: false,
            wal_upload_buffer_bytes: 8 * MIB,
            s3_endpoint: "127.0.0.1:9000".to_string(),
            s3_bucket: "pos3ql".to_string(),
            s3_prefix: String::new(),
            s3_region: "us-east-1".to_string(),
            s3_access_key: String::new(),
            s3_secret_key: String::new(),
            s3_head_bytes: 16 * KIB,
            s3_response_bytes: 4 * MIB,
        }
    }

    /// Parses `key = value` lines over the development defaults.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut config = Self::default_dev();
        let mut seen: Vec<String> = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line_no = index + 1;
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::at(line_no, format!("expected key = value, got '{line}'")));
            };
            let key = key.trim();
            let value = value.trim();
            if seen.iter().any(|s| s == key) {
                return Err(ConfigError::at(line_no, format!("duplicate key '{key}'")));
            }
            seen.push(key.to_string());
            match key {
                "listen_addr" => config.listen_addr = value.to_string(),
                "data_dir" => config.data_dir = value.to_string(),
                "max_connections" => config.max_connections = parse_count(value).map_err(|m| ConfigError::at(line_no, m))?,
                "auth" => {
                    if !matches!(value, "trust" | "password" | "scram-sha-256") {
                        return Err(ConfigError::at(
                            line_no,
                            format!("auth must be trust, password or scram-sha-256, got '{value}'"),
                        ));
                    }
                    config.auth = value.to_string();
                }
                "password" => config.password = value.to_string(),
                "replica_id" => config.replica_id = parse_count(value).map_err(|m| ConfigError::at(line_no, m))?,
                "cluster" => {
                    config.cluster = value
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                "conn_recv_buffer_bytes" => config.conn_recv_buffer_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "conn_send_buffer_bytes" => config.conn_send_buffer_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "sql_arena_bytes" => config.sql_arena_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "work_arena_bytes" => config.work_arena_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "max_prepared" => config.max_prepared = parse_count(value).map_err(|m| ConfigError::at(line_no, m))? as usize,
                "prepared_bytes" => config.prepared_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "max_portals" => config.max_portals = parse_count(value).map_err(|m| ConfigError::at(line_no, m))? as usize,
                "portal_bytes" => config.portal_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "portal_result_bytes" => config.portal_result_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "txn_rows" => config.txn_rows = parse_count(value).map_err(|m| ConfigError::at(line_no, m))? as usize,
                "memtable_bytes" => config.memtable_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "wal_bytes" => config.wal_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "wal_buffer_bytes" => config.wal_buffer_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "max_tables" => config.max_tables = parse_count(value).map_err(|m| ConfigError::at(line_no, m))? as usize,
                "max_cursors" => config.max_cursors = parse_count(value).map_err(|m| ConfigError::at(line_no, m))? as usize,
                "cursor_bytes" => config.cursor_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "table_rows" => config.table_rows = parse_count(value).map_err(|m| ConfigError::at(line_no, m))? as usize,
                "block_cache_bytes" => config.block_cache_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "disk_cache_bytes" => config.disk_cache_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "wal_upload" => {
                    config.wal_upload = match value {
                        "on" | "true" => true,
                        "off" | "false" => false,
                        other => {
                            return Err(ConfigError::at(
                                line_no,
                                format!("wal_upload must be on or off, got '{other}'"),
                            ))
                        }
                    }
                }
                "wal_upload_sync" => {
                    config.wal_upload_sync = match value {
                        "on" | "true" => true,
                        "off" | "false" => false,
                        other => {
                            return Err(ConfigError::at(
                                line_no,
                                format!("wal_upload_sync must be on or off, got '{other}'"),
                            ))
                        }
                    }
                }
                "wal_upload_buffer_bytes" => config.wal_upload_buffer_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "s3" => {
                    config.s3_on = match value {
                        "on" | "true" => true,
                        "off" | "false" => false,
                        other => {
                            return Err(ConfigError::at(
                                line_no,
                                format!("s3 must be on or off, got '{other}'"),
                            ))
                        }
                    }
                }
                "s3_endpoint" => config.s3_endpoint = value.to_string(),
                "s3_bucket" => config.s3_bucket = value.to_string(),
                "s3_prefix" => config.s3_prefix = value.to_string(),
                "s3_region" => config.s3_region = value.to_string(),
                "s3_access_key" => config.s3_access_key = value.to_string(),
                "s3_secret_key" => config.s3_secret_key = value.to_string(),
                "s3_head_bytes" => config.s3_head_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                "s3_response_bytes" => config.s3_response_bytes = parse_size(value).map_err(|m| ConfigError::at(line_no, m))?,
                _ => return Err(ConfigError::at(line_no, format!("unknown key '{key}'"))),
            }
        }
        Ok(config)
    }

    /// RAM the server will reserve at startup, itemized. Disk budgets are
    /// reported separately by the caller. `server_bytes` and `tables_bytes`
    /// cover machinery sized by the caller (reactor buffers, table maps).
    pub fn memory_plan(&self, server_bytes: usize, tables_bytes: usize) -> MemoryPlan {
        let per_connection = self.conn_recv_buffer_bytes
            + self.conn_send_buffer_bytes
            + self.sql_arena_bytes
            + self.max_prepared * self.prepared_bytes
            + self.max_portals * (self.portal_bytes + self.portal_result_bytes)
            + crate::sql::cursor::CursorPool::budget_bytes(self)
            + self.txn_rows * 12;
        MemoryPlan {
            memtable: self.memtable_bytes,
            tables: tables_bytes,
            block_cache: self.block_cache_bytes,
            connections: per_connection * self.max_connections as usize,
            server: server_bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryPlan {
    pub memtable: usize,
    pub tables: usize,
    pub block_cache: usize,
    pub connections: usize,
    pub server: usize,
}

impl MemoryPlan {
    pub fn total(&self) -> usize {
        self.memtable + self.tables + self.block_cache + self.connections + self.server
    }
}

impl fmt::Display for MemoryPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "memory plan:")?;
        writeln!(f, "  memtable     {:>12}", FmtBytes(self.memtable))?;
        writeln!(f, "  tables       {:>12}", FmtBytes(self.tables))?;
        writeln!(f, "  block cache  {:>12}", FmtBytes(self.block_cache))?;
        writeln!(f, "  connections  {:>12}", FmtBytes(self.connections))?;
        writeln!(f, "  server       {:>12}", FmtBytes(self.server))?;
        write!(f, "  total        {:>12}", FmtBytes(self.total()))
    }
}

/// Renders byte counts with binary suffixes for the plan printout.
pub struct FmtBytes(pub usize);

impl fmt::Display for FmtBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.0;
        if value >= GIB && value.is_multiple_of(GIB) {
            write!(f, "{} GiB", value / GIB)
        } else if value >= MIB && value.is_multiple_of(MIB) {
            write!(f, "{} MiB", value / MIB)
        } else if value >= KIB && value.is_multiple_of(KIB) {
            write!(f, "{} KiB", value / KIB)
        } else {
            write!(f, "{value} B")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub line: usize,
    pub message: String,
}

impl ConfigError {
    fn at(line: usize, message: String) -> Self {
        Self { line, message }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "config line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ConfigError {}

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const GIB: usize = 1024 * MIB;

/// Parses `4096`, `64KiB`, `16 MiB`, `2GiB`.
fn parse_size(value: &str) -> Result<usize, String> {
    let value = value.trim();
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (digits, suffix) = value.split_at(split);
    let number: usize = digits
        .parse()
        .map_err(|_| format!("invalid size '{value}'"))?;
    let unit = match suffix.trim() {
        "" => 1,
        "KiB" => KIB,
        "MiB" => MIB,
        "GiB" => GIB,
        other => return Err(format!("unknown size suffix '{other}' (use KiB, MiB, GiB)")),
    };
    number
        .checked_mul(unit)
        .ok_or_else(|| format!("size '{value}' overflows"))
}

fn parse_count(value: &str) -> Result<u32, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("invalid count '{value}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        assert_eq!(Config::parse("").unwrap(), Config::default_dev());
        assert_eq!(
            Config::parse("# just a comment\n\n").unwrap(),
            Config::default_dev()
        );
    }

    #[test]
    fn parses_keys_sizes_and_comments() {
        let text = "\
# development overrides
listen_addr = 0.0.0.0:5432
max_connections = 128
memtable_bytes = 16MiB   # small for tests
sql_arena_bytes = 4096
";
        let c = Config::parse(text).unwrap();
        assert_eq!(c.listen_addr, "0.0.0.0:5432");
        assert_eq!(c.max_connections, 128);
        assert_eq!(c.memtable_bytes, 16 * MIB);
        assert_eq!(c.sql_arena_bytes, 4096);
        // Untouched keys keep defaults.
        assert_eq!(c.block_cache_bytes, Config::default_dev().block_cache_bytes);
    }

    #[test]
    fn unknown_key_is_an_error_with_line_number() {
        let err = Config::parse("listen_addr = x\nmax_conections = 4\n").unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.message.contains("unknown key 'max_conections'"), "{err}");
    }

    #[test]
    fn duplicate_key_is_an_error() {
        let err = Config::parse("memtable_bytes = 1MiB\nmemtable_bytes = 2MiB\n").unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.message.contains("duplicate"), "{err}");
    }

    #[test]
    fn malformed_values_are_errors() {
        assert!(Config::parse("memtable_bytes = 16MB\n").is_err(), "MB is not MiB");
        assert!(Config::parse("memtable_bytes = lots\n").is_err());
        assert!(Config::parse("max_connections = -1\n").is_err());
        assert!(Config::parse("just some words\n").is_err());
    }

    #[test]
    fn memory_plan_math() {
        let mut c = Config::default_dev();
        c.max_connections = 10;
        c.conn_recv_buffer_bytes = 100;
        c.conn_send_buffer_bytes = 200;
        c.sql_arena_bytes = 300;
        c.max_prepared = 2;
        c.prepared_bytes = 30;
        c.max_portals = 2;
        c.portal_bytes = 20;
        c.portal_result_bytes = 40;
        c.txn_rows = 10;
        c.memtable_bytes = 1000;
        c.block_cache_bytes = 2000;
        c.max_cursors = 1;
        c.cursor_bytes = 64;
        let plan = c.memory_plan(500, 250);
        // 100+200+300 + 2*30 + 2*(20+40) + cursor pool + 10*12 per connection.
        let cursor_pool = crate::sql::cursor::CursorPool::budget_bytes(&c);
        assert_eq!(plan.connections, (900 + cursor_pool) * 10);
        assert_eq!(plan.total(), (900 + cursor_pool) * 10 + 1000 + 2000 + 500 + 250);
    }

    #[test]
    fn size_formatting_roundtrips() {
        assert_eq!(FmtBytes(3 * GIB).to_string(), "3 GiB");
        assert_eq!(FmtBytes(64 * MIB).to_string(), "64 MiB");
        assert_eq!(FmtBytes(4 * KIB).to_string(), "4 KiB");
        assert_eq!(FmtBytes(999).to_string(), "999 B");
    }
}
