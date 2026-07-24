# pos3ql

A PostgreSQL-compatible database engine whose durable storage is S3-compatible
object storage (AWS S3, MinIO, or any endpoint speaking the S3 API), written
in Rust with TigerBeetle-style engineering discipline.

## Design pillars

- **PostgreSQL compatibility for real clients.** The wire protocol (v3,
  simple *and* extended query) and the SQL dialect follow PostgreSQL so that
  psql, JDBC, npgsql, psycopg, node-postgres, etc. work — including the
  introspection queries drivers issue on connect.
- **Object storage is the durable home.** Content-addressed, checksummed
  block SSTs, WAL segments, and the CAS'd manifest live in an S3-compatible
  bucket, and a node can cold-start from an empty disk. Reads go **RAM block
  cache → local disk cache → ranged GET** (`block_cache_bytes` /
  `disk_cache_bytes`), and under memory pressure committed row bytes spill to
  the bucket and page back through the caches, so ingest is not bounded by
  RAM *bytes*. The remaining RAM bound is the per-row index (row *count*),
  and commit durability is still the local WAL first — closing both is the
  *Maturity roadmap* in [PLAN.md](PLAN.md).
- **Static allocation.** All memory is acquired at startup, sized from
  config. No heap allocation after init — enforced by a guarding global
  allocator. Every pool and queue has a fixed limit; exhaustion is a loud
  error, never growth.
- **Deterministic core.** The replica is a sans-io state machine driven by an
  event loop (kqueue/epoll). The same core runs under a deterministic
  simulator with fault injection (VOPR-style), so cluster bugs reproduce from
  a seed.
- **1..N replicas.** Consensus is Viewstamped Replication (the protocol
  TigerBeetle uses); a single node is a cluster of one. Commit latency is
  quorum-disk latency; object-storage upload is asynchronous to commit.

## Dependency policy

`std` + `libc` only (raw syscall bindings). No async runtime, no protocol or
parser crates, no cloud SDKs. TLS is never hand-rolled: the one whitelisted
exception is an **isolated rustls component** for HTTPS to the object store
(`s3_tls = on`, optional `s3_tls_ca_file` for self-signed endpoints) — every
rustls call runs inside a budgeted allocator scope (`tls_pool_bytes`) so the
static-memory discipline holds everywhere else.

## Status

Working single-node database:

- psql 18 and psycopg 3 connect and work — wire protocol 3.0 **and 3.2**,
  simple and extended query protocol (including binary parameters and named
  prepared statements).
- SQL: DDL (CREATE/DROP TABLE, CREATE/DROP VIEW, CREATE/DROP INDEX),
  INSERT/SELECT/UPDATE/DELETE with WHERE / ORDER BY (PostgreSQL null ordering)
  / LIMIT, joins, GROUP BY and aggregates, subqueries (correlated + EXISTS),
  non-recursive CTEs, updatable views, arbitrary-precision NUMERIC, casts and
  scalar functions, plan-time type analysis, `pg_catalog` / `information_schema`
  introspection, PostgreSQL lexical rules, and SQLSTATE-correct errors.
- Transactions: BEGIN/COMMIT/ROLLBACK with READ COMMITTED snapshot isolation,
  transactional DDL, and fail-fast (`40001`) write-conflict detection.
- Durability: CRC-checksummed WAL with F_FULLFSYNC (kill -9 safe); CHECKPOINT
  snapshots every table to the bucket behind a compare-and-swap manifest, a
  node with an empty disk cold-starts entirely from it, and `wal_upload`
  streams WAL segments to the bucket (asynchronously by default). See
  **Durability and write safety** below.
- `tests/external/run.sh` runs the external conformance suite against real
  MinIO (psql golden files, raw wire probes, psycopg driver suite, kill-9 and
  cold-start durability scenarios, differential vs PostgreSQL 18).

Not yet: multi-replica VSR and client-facing TLS (TLS to the bucket is in;
the server's SSLRequest answer is still `N`). See [PLAN.md](PLAN.md) for the
roadmap and
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
| `s3 = off` (or `wal_upload = off`) | local fsync | yes (WAL replay) | only up to the last `CHECKPOINT` snapshot in the bucket |
| `wal_upload = on`, `wal_upload_sync = off` | local fsync | yes | **eventually** — the S3 upload is drained off the commit path, so a transaction committed within the last drain window is lost from S3 if the disk is also lost in that window |
| `wal_upload_sync = on` (**default with s3 = on**) | local fsync **+ S3 round-trip** | yes | yes (RPO=0 to S3 — the batch is in the bucket before the ack) |
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
- **DDL isolation covers tables only.** Uncommitted `CREATE`/`DROP TABLE` is
  invisible to other sessions until commit (catalog MVCC); uncommitted
  `CREATE`/`DROP VIEW` and `CREATE`/`DROP INDEX` still apply to the shared
  catalog immediately (B-016). A concurrent CREATE/DROP of the same name is
  reported as `40001` rather than blocking on the catalog lock.
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
- **The row index must fit RAM.** Row *bytes* spill to the bucket under
  memory pressure and page back through the RAM/disk caches, but the per-row
  map (visibility, uniqueness, bookkeeping) stays in RAM, so dataset size is
  bounded in row *count* (`table_rows`) rather than bytes. Making the map
  itself block-resident is gap 2 of the *Maturity roadmap* in
  [PLAN.md](PLAN.md).
- **Fixed capacities.** Connections, tables, columns, prepared statements,
  transaction footprint, and every buffer are sized from config at startup;
  exceeding any is a loud error, never silent growth.
- **No client-facing TLS.** The object-store side speaks HTTPS
  (`s3_tls = on`, isolated rustls); the PostgreSQL wire side still answers
  the SSLRequest probe with `N` — server-side TLS is in the maturity
  roadmap's compatibility wave.

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
