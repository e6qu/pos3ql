//! PostgreSQL wire protocol (v3) frontend.

pub mod auth;
pub mod conn;
pub mod respond;
pub mod tls;
pub mod wire;

/// Version reported via ParameterStatus and `SHOW server_version`.
/// PostgreSQL-compatible databases report a concrete PostgreSQL version so
/// clients can gate features; this tracks the release the wire and SQL
/// dialects target.
pub const REPORTED_SERVER_VERSION: &str = "18.4 (pos3ql 0.1)";

/// The same version as a PostgreSQL `server_version_num` (major × 10000 + minor
/// — `18.4` → `180004`). Clients read it to gate features numerically.
pub const REPORTED_SERVER_VERSION_NUM: &str = "180004";
