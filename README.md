# pos3ql

A PostgreSQL-compatible database engine whose durable storage is a
provider-neutral object store (AWS S3, MinIO, Google Cloud Storage, Azure Blob
Storage, or an equivalent adapter), written in Rust with TigerBeetle-style
engineering discipline.

## Design pillars

- **PostgreSQL compatibility for real clients.** The wire protocol (v3,
  simple *and* extended query) and the SQL dialect follow PostgreSQL so that
  psql, JDBC, npgsql, psycopg, node-postgres, etc. work — including the
  introspection queries drivers issue on connect.
- **Object storage is the durable home.** Content-addressed, checksummed
  block SSTs, WAL segments, and the CAS'd manifest live behind one semantic
  object-store contract, and a node can cold-start from an empty disk. The
  first transport speaks the S3-compatible API; native adapters or compatibility
  gateways provide the same immutable-object, range-read, listing, deletion,
  and conditional-root semantics for other providers without storage-engine
  special cases. Reads go **RAM block
  cache → local disk cache → ranged GET** (`block_cache_bytes` /
  `disk_cache_bytes`), and under memory pressure committed row bytes spill to
  the bucket and page back through the caches, so ingest is not bounded by
  RAM *bytes* or total row count. With object storage enabled, synchronous WAL
  upload is the default, so acknowledged data is already in the durable tier
  and local disk is disposable. The remaining storage bound is the in-RAM uniqueness index
  on constrained tables; its persistent secondary-index forest is tracked in
  the *Maturity roadmap* in [PLAN.md](PLAN.md).
- **Static allocation.** All memory is acquired at startup, sized from
  config. No heap allocation after init — enforced by a guarding global
  allocator. Every pool and queue has a fixed limit; exhaustion is a loud
  error, never growth.
- **Deterministic core.** The replica is a sans-io state machine driven by an
  event loop (kqueue/epoll). The same core runs under a deterministic
  simulator with fault injection (VOPR-style), so cluster bugs reproduce from
  a seed.
- **1..N replicas.** Consensus is Viewstamped Replication (the protocol
  TigerBeetle uses); a single node is a cluster of one. The production server
  is single-node today and synchronously uploads WAL by default when object
  storage is enabled; live quorum write-routing remains roadmap work.

## Dependency policy

`std` + `libc` only (raw syscall bindings). No async runtime, no protocol or
parser crates, no cloud SDKs. TLS is never hand-rolled: the one whitelisted
exception is an **isolated rustls component** for HTTPS to the object store
(`object_store_tls = on`, optional `object_store_tls_ca_file` for self-signed
endpoints) — every
rustls call runs inside a budgeted allocator scope (`tls_pool_bytes`) so the
static-memory discipline holds everywhere else.

The documented durable-storage settings use the `object_store*` prefix.
Existing `s3*` settings remain strict compatibility aliases; configuring both
names for one setting is an error rather than an override.

## Status

Working single-node database:

- psql 18 and psycopg 3 connect and work — wire protocol 3.0 **and 3.2**,
  simple and extended query protocol (including binary parameters and named
  prepared statements).
- SQL: DDL (CREATE/DROP TABLE, CREATE TABLE AS / SELECT ... INTO, CREATE/DROP VIEW,
  CREATE/REFRESH/DROP MATERIALIZED VIEW, CREATE/ALTER/DROP SEQUENCE
  (nextval/currval/lastval/setval), CREATE/DROP INDEX, COMMENT ON
  TABLE/VIEW/MATERIALIZED VIEW/INDEX/SEQUENCE/COLUMN/SCHEMA/TYPE/DOMAIN
  (including view columns and relation composite types), read back through
  obj_description/col_description/pg_description),
  INSERT/SELECT/UPDATE/DELETE/MERGE with WHERE / ORDER BY (PostgreSQL null
  ordering) / LIMIT / OFFSET / FETCH FIRST ... WITH TIES, row-locking clauses
  (`FOR UPDATE`/`SHARE`/`NO KEY UPDATE`/`KEY SHARE` with `OF`/`NOWAIT`/`SKIP
  LOCKED`), INSERT ... ON CONFLICT
  (`DO NOTHING` / `DO UPDATE` upsert with column- or `ON CONSTRAINT`-inferred
  arbiters, `excluded.*`, and RETURNING), joins (including
  `LATERAL` subqueries and set-returning functions), GROUP BY and aggregates,
  subqueries (correlated + EXISTS),
  non-recursive and recursive CTEs (including data-modifying CTEs — ordinary,
  recursive, and modifying entries chain left-to-right into a `SELECT`,
  `INSERT`, `UPDATE`, `DELETE`, or `MERGE` main statement under PostgreSQL's
  single-command snapshot), updatable views, constant and expression column DEFAULTs
  (`DEFAULT now()` / `DEFAULT nextval(...)` evaluated per row), generated columns
  (`GENERATED ALWAYS AS (expr) STORED`), identity columns
  (`GENERATED ALWAYS/BY DEFAULT AS IDENTITY`), arbitrary-precision NUMERIC,
  network address types (`inet`/`cidr`/`macaddr`/`macaddr8` with their operators
  and functions), user-defined domains (`CREATE DOMAIN` with NOT NULL / DEFAULT /
  CHECK, recursive domains, casts, generated arrays, and ALTER validation),
  user-defined enum types (`CREATE TYPE ... AS ENUM` with arrays, ADD/RENAME
  VALUE and RENAME TO, ordered by definition order and reflected in
  `pg_type`/`pg_enum`), casts and
  scalar functions, plan-time type analysis, `pg_catalog` / `information_schema`
  introspection (including psql's detailed table/view/materialized-view/index/
  sequence/domain/type displays and standard object listings), transactional
  `SHOW` / `SET` / `SET LOCAL` / `RESET` /
  `RESET ALL` and `current_setting(...)` / `set_config(...)` for session
  settings (including savepoint rollback),
  PostgreSQL lexical rules, and SQLSTATE-correct errors.
- COPY FROM STDIN / TO STDOUT (including `COPY (query) TO STDOUT`) over both
  the simple and extended query protocols, in
  PostgreSQL's text, CSV, and binary formats —
  psql `\copy` and pg_dump-style inline data streams work, with each type's
  input and output functions, expression/sequence defaults, generated columns,
  and full constraint enforcement. The binary format
  is byte-exact against PostgreSQL for the whole type surface, composites
  included (arrays, ranges, multiranges, bit strings, network addresses).
- PostgreSQL dumps restore through both psql and pg_restore: PostgreSQL 18
  plain SQL and ownerful custom archives cover schemas, enum/domain types,
  generated and identity columns, owned sequence positions, constraints, btree
  indexes, views and materialized views. CI runs parallel pg_restore, replaces
  the populated catalog with `--clean --if-exists`, and verifies the result
  again after restart.
- Transactions: BEGIN/COMMIT/ROLLBACK with READ COMMITTED snapshot isolation,
  transactional DDL, and fail-fast (`40001`) write-conflict detection.
- LISTEN / NOTIFY: `LISTEN`/`UNLISTEN`/`NOTIFY channel[, payload]` with
  PostgreSQL's transactional delivery (fired at commit, dropped on rollback,
  de-duplicated within a transaction) and asynchronous cross-connection
  NotificationResponse delivery.
- TLS: server-side TLS for client connections (`tls_on` with `tls_cert_file` /
  `tls_key_file`) — `sslmode=require` negotiates TLS 1.3 via the isolated rustls
  component; clients that do not request TLS still connect in the clear.
- Durability: CRC-checksummed WAL with F_FULLFSYNC (kill -9 safe); CHECKPOINT
  snapshots every table to the bucket behind a compare-and-swap manifest, a
  node with an empty disk cold-starts entirely from it, and `wal_upload`
  streams WAL segments to the bucket (synchronously by default with object
  storage enabled). See
  **Durability and write safety** below.
- `tests/external/run.sh` runs the external conformance suite against real
  MinIO (psql golden files, raw wire probes, psycopg driver suite, kill-9 and
  cold-start durability scenarios, differential vs PostgreSQL 18).

Not yet: multi-replica VSR. See [PLAN.md](PLAN.md) for the roadmap and
[BUGS.md](BUGS.md) for known divergences; the headline ones are summarized
under **Limitations** below. [AGENTS.md](AGENTS.md) holds the standing
directives, and [docs/terminology.md](docs/terminology.md) is the glossary and
naming rules.

## Durability and write safety

A committed transaction is always made durable on **local disk** before the
client is acknowledged: the WAL is CRC-checksummed and fsynced with
`F_FULLFSYNC` (macOS) / `fdatasync` (Linux), so a process crash, `kill -9`, or
power loss replays cleanly on restart (to the extent the disk honors the sync).
That is the floor and it is not configurable.

Durability *against loss of the local disk itself* is tiered by configuration:

| Mode | Commit latency | Survives process crash | Survives total local-disk loss |
|------|----------------|------------------------|--------------------------------|
| `object_store = off` (or `wal_upload = off`) | local fsync | yes (WAL replay) | only up to the last `CHECKPOINT` snapshot in the durable object namespace |
| `wal_upload = on`, `wal_upload_sync = off` | local fsync | yes | **eventually** — object upload is drained off the commit path, so a transaction committed within the last drain window is lost from the durable tier if the disk is also lost in that window |
| `wal_upload_sync = on` (**default with object storage on**) | local fsync **+ object-store round-trip** | yes | yes (RPO=0 — the batch is in the durable tier before the ack) |
| Multi-replica VSR | quorum-disk | yes | yes (quorum) | *(not yet active — see PLAN.md)* |

`CHECKPOINT` snapshots every table to the bucket behind a compare-and-swap
manifest; a node with a wiped disk cold-starts entirely from the last snapshot
plus any newer uploaded WAL segments. **Commit-durable-on-bucket is the
default whenever object storage is on**: the local disk is a mere cache, so an
acknowledged commit must not live only there. Set `wal_upload_sync = off` to
trade that for local-fsync commit latency with an asynchronous drain; the
low-latency path to RPO=0 is VSR replication, not single-node synchronous
upload.

## Limitations

Known divergences from PostgreSQL and current constraints (details and IDs in
[BUGS.md](BUGS.md)):

- **Concurrency is single-threaded, fail-fast.** Isolation is READ COMMITTED;
  sessions interleave only at message boundaries. A write-write conflict fails
  immediately with `40001` (serialization failure) — pos3ql does **not**
  block-and-wait like PostgreSQL READ COMMITTED, so applications must retry
  (B-004).
- **Catalog conflicts are fail-fast.** Tables, views, materialized views,
  indexes, schemas, sequences, domains, and enums all participate in
  transactional catalog MVCC and savepoint rollback. A concurrent DDL change
  to the same object reports `40001` rather than blocking on a catalog lock.
- **Sorts are bounded by a `work_mem` analogue.** `ORDER BY` / `DISTINCT` /
  `GROUP BY` materialize in a fixed shared arena (`work_arena_bytes`, 64 MiB
  default — larger than PostgreSQL's 4 MiB default `work_mem`). A result that
  exceeds it errors `54000` rather than spilling to temporary files (B-006).
- **A checkpoint beat blocks for one table's write.** The auto-checkpoint is
  sliced — one table's SSTs per beat, beats interleaved with statements and
  driven on by the idle event loop, publishing only when no table changed
  since its slice — so a checkpoint no longer stalls connections for its
  whole duration, but a single very large table's slice still blocks while
  it writes (per-block beats are the roadmap's Stage E). The explicit
  `CHECKPOINT` statement and the cold-start load remain atomic.
- **The row map is an overlay, not an index.** `table_rows` bounds the
  *working set* — pending changes plus rows not yet shed under pressure —
  not the dataset: rows beyond it live only in the bucket's SSTs and are
  read back through bloom-gated probes and merged walks. A single
  transaction's touched rows must still fit the map, and with `object_store = off`
  (no bucket) the map bound is the table bound.
- **Uniqueness is value-indexed.** A `PRIMARY KEY` / `UNIQUE` constraint keeps
  an in-RAM `value → rowid` index, so a duplicate check is a hash seek rather
  than a scan of the whole spilled dataset. It bounds a constrained table to
  `value_index_rows` committed rows (a loud error past it) — the price of an
  in-RAM index; lifting that bound for unboundedly-spilling *constrained* tables
  is the persistent index forest on the roadmap.
- **Fixed capacities.** Connections, tables, columns, prepared statements,
  transaction footprint, and every buffer are sized from config at startup;
  exceeding any is a loud error, never silent growth.
- **TLS is opt-in.** Object-store HTTPS is controlled by `object_store_tls`; PostgreSQL
  wire TLS is enabled with `tls_on`, `tls_cert_file`, and `tls_key_file`.
  Cleartext clients remain accepted when server TLS is configured.

## Quick start

```sh
docker run -d -p 19100:9000 -e MINIO_ROOT_USER=minioadmin \
  -e MINIO_ROOT_PASSWORD=minioadmin minio/minio server /data
docker exec <container> mc mb local/pos3ql   # after: mc alias set local ...
cargo run --release -- --config examples/dev.conf
psql -h 127.0.0.1 -p 5433 -U you
```

## References

- PostgreSQL Frontend/Backend Protocol: https://www.postgresql.org/docs/current/protocol.html
- Viewstamped Replication Revisited (Liskov & Cowling, 2012): https://pmg.csail.mit.edu/papers/vr-revisited.pdf
- TigerBeetle safety/design docs: https://docs.tigerbeetle.com/concepts/safety/
- AWS Signature Version 4: https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html
