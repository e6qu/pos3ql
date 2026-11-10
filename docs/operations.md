# Operations

This guide covers the operational listener, scheduled work, monitoring, logs,
credential rotation, and replacing the active writer. Cross-machine failover
with real client routing is not yet qualified; see
[Qualify production deployment](../PLAN.md#qualify-production-deployment).

**User example:** a process can answer a local health check while it cannot
save new commits. Route database work only to a process whose readiness check
succeeds; being alive does not prove durable storage works or that the process
still owns writes.

## Configuration

`pos3ql --config <path>` is required. Every limit is fixed at startup: the
server reserves the memory the file implies and returns a named error when a
pool is exhausted. Sizes accept `KiB`, `MiB`, and `GiB`. A missing required
setting fails startup. Full examples are `examples/dev.conf` in the source
tree and the packaged `/etc/pos3ql/pos3ql.conf`; physical limits are in
[memory and physical size](architecture.md#memory-and-physical-size).

| Setting | Required | Bounds |
|---|---|---|
| `transaction_mode` | yes | `mvcc` keeps older row versions for readers; `locking` makes conflicting work wait. |
| `catalog_history_bytes` | yes | Encoded catalog history. Nonzero (e.g. `16MiB`) in `mvcc`; `0` in `locking`. |
| `row_history_slots` | yes | Committed row-history slots shared by all tables and large objects. Nonzero (e.g. `65536`) in `mvcc`; `0` in `locking`. |
| `row_overlay_slots` | yes | Resident row metadata shared by ordinary tables in all databases. |
| `row_work_bytes` | yes | Shared foreground bytes for retained row identities and DML buffers; a multiple of 64. |
| `row_work_leases` | yes | Concurrent leases on `row_work_bytes`. |
| `execution_control_bytes` | yes | Bytes for live procedural-execution control records. |
| `execution_control_leases` | yes | Concurrent owned ranges in `execution_control_bytes`. |
| `definition_image_slots` | yes | Identities for retained table schemas and synthetic query columns. |
| `table_metadata_bytes` | yes | Bytes for table definitions and index bindings. |
| `table_metadata_leases` | yes | Concurrent owned ranges in `table_metadata_bytes`. |
| `statistics_bytes` | yes | Bytes for analyzed-column statistics. |
| `statistics_leases` | yes | Concurrent owned ranges in `statistics_bytes`, including retained images. |
| `index_statistics_slots` | yes | Index lifetimes sharing activity counters, including retained readers. |
| `result_spool_bytes` | yes | Shared bytes queuing result rows for clients that read slower than statements produce them; a multiple of 64 (e.g. `16MiB`). A client further behind gets `54000` naming this setting after the rows already queued. |
| `result_spool_leases` | yes | Connections that can hold queued result rows at once (e.g. `max_connections`). |
| `result_stream_bytes` | yes | Per-connection query, parameters and read positions of a large result that resumes row by row as its client reads (e.g. `256KiB`); at least 64 KiB, or `0` to disable resuming so such results queue in the spool. |
| `large_object_pages` | no (default `8192`, must be nonzero) | Resident 2 KiB large-object pages. |

## Operational listener

A separate HTTP listener serves health probes, Prometheus metrics, and startup
capacity. It is disabled by default. Enable it on a private interface:

```text
operations_listen_addr = 127.0.0.1:9187
operations_max_connections = 4
log_format = json
```

The listener is plaintext and unauthenticated. Do not expose it to an
untrusted network; put TLS and access control in front of it when remote
access is needed. Its connection slots, 2 KiB request buffers and 16 KiB
response buffers are reserved at startup, so probes do not count against
`max_connections` and cannot grow runtime memory.

```sh
curl --fail http://127.0.0.1:9187/livez
curl --fail http://127.0.0.1:9187/readyz
curl --fail http://127.0.0.1:9187/capacity
```

| Path | Status | Meaning |
|---|---:|---|
| `/livez`, `/healthz` | 200 | The event loop answered. |
| `/readyz` | 200 | Startup finished, durable progress is healthy, and this process still owns the writer fence. |
| `/readyz` | 503 | Journal (WAL) publication or remote checkpoint work failed, object storage is unavailable, or another process owns the writer fence. |
| `/metrics` | 200 | Prometheus text: readiness, connections, query workspaces, active leases and waiters, WAL, row memory, checkpoint pressure, cache traffic, immutable-block object requests, result streams and spooled results by reason. |
| `/capacity` | 200 | JSON: current use, waiting work, and startup limits for memory, connections, query workspaces, WAL, row heap, caches, catalogs, prepared transactions, replication, and whether credential rotation is configured. |

Only `GET` over HTTP/1.0 or HTTP/1.1 is accepted; other methods and unknown
paths fail explicitly. Responses are uncacheable and close the connection.
More connections allow more waiting work; SQL execution still uses one CPU
worker.

### Readiness

The writer fence is the object-store record that names the one process
allowed to publish commits. In durable mode, `/readyz` and `/metrics` read and
validate it, so a displaced idle process stops reporting ready before it
serves more routed reads. A reserved storage client performs this check;
concurrent checks may share one unfinished read, but a new check never reuses
an earlier result. Transport failures retry within the usual three-attempt,
30-second request limit. Slow storage delays only these responses: cached SQL
queries, `/livez`, and `/capacity` keep answering.

Readiness becomes false when commit publication or remote checkpoint work
fails, and true again only after a successful durable publication or
completion of the pending remote cleanup. Skipped maintenance, local heap
cleanup, or a temporary-file spill does not clear it. A cached report can
therefore succeed while `/readyz` still reports the failed publication. Local
capacity errors return their named SQL errors and do not change readiness.

Failed background maintenance retries no sooner than one second later; client
traffic does not shorten the delay, and an idle server retries too. The commit
barrier, not this pacing, decides when client responses are durable.

## Scheduled work and schema migrations

Checkpointing, file merges, row-history cleanup, and eligible object deletion
run inside the server between client work and while idle; no OS cron job is
needed. COMMIT waits for its durable journal publication, not for a full
checkpoint. See [maintenance and scheduling](architecture.md#maintenance-and-scheduling).

Application jobs need an external scheduler (OS cron or your job runner)
connecting as an ordinary PostgreSQL client. There is no built-in SQL job
scheduler or background-worker interface, and `pg_cron` is not a qualified
extension. Example daily job:

```cron
0 2 * * * PGSERVICE=orders_jobs /usr/bin/psql -X -v ON_ERROR_STOP=1 -f /etc/orders/remove_expired_orders.sql
```

`orders_jobs` is a libpq service you define (database, host, TLS, account),
not a pos3ql setting. Keep passwords in an owner-only file, and let the
scheduler record failures and prevent overlapping runs. Keep transactions
small enough for the configured buffers. A retry after a lost commit reply
must first check whether the work already happened.

Migration tools also connect as ordinary clients. pos3ql provides
transactional DDL and savepoints but no migration runner or history table, so
the tool must choose one runner and record applied versions. For example:

```sql
BEGIN;
SET LOCAL lock_timeout = '5s';
ALTER TABLE orders ADD COLUMN fulfilled_at timestamptz;
COMMIT;
```

A conflicting lock fails this with `55P03`; roll back before retrying. ADD
COLUMN, CREATE INDEX, constraint validation, and type rewrites differ in
locking, row scans, and memory. Rehearse the real SQL and data size under the
intended startup capacities.

Upgrading the pos3ql binary is separate: older pos3ql formats are not
converted automatically. Check the [format matrix](durable-format.md), and
when needed verify a logical export and restore into a fresh prefix while
keeping the source. [Offline backup commands](backup-restore.md) take writer
ownership and fence a running server on that prefix; never schedule them
against a live server as if they were online SQL backups.

## Metrics and alerts

Scrape `/metrics` from the private network. Suggested first alerts:

| Signal | Action |
|---|---|
| `pos3ql_up == 0` | Process or operational listener unreachable. |
| `pos3ql_ready == 0` | Stop routing new work; inspect publication errors. |
| `pos3ql_object_store_credential_reload_failures_total` increases | Fix the credential file and reload. |
| Connections near `pos3ql_postgres_connection_capacity` | Raise the startup limit or reduce client concurrency. |
| WAL or row-heap use near capacity | Check checkpoint progress and object-store latency. |
| Sustained object read latency, cache misses, or prefetch saturation | Compare the working set with cache and object-read slot capacities. |
| `pos3ql_result_stream_spooled_total{reason="capacity"}` grows | Large simple results queue in the spool because their read position exceeds `result_stream_bytes`; raise it or checkpoint more often. |

Counters reset at process start. Block-object request counters cover
immutable SST traffic through the cache stack only, not root, commit-batch,
LIST, DELETE, or storage-service-internal requests.

## Logs

`log_format = text` gives human-readable lines. `log_format = json` emits one
JSON object per line with `timestamp_unix_ms`, `level`, `event`, and
`message`. Database credentials and object-store secrets are never logged.

## Object-store credential rotation

Prefer `object_store_credentials_file` to inline credential settings. The file
must be a regular UTF-8 file of at most 4096 bytes with no group or other
permissions:

```text
access_key = example-access
secret_key = example-secret
session_token = optional-temporary-token
```

To rotate without interrupting durable work:

1. Grant the new credential access to the bucket and prefix while the old one
   remains valid.
2. Write the new file beside the configured path with mode `0600`, then
   rename it over the configured path.
3. Send `SIGHUP` or run `SELECT pg_reload_conf()`.
4. Wait for `/readyz` to return 200 and
   `pos3ql_object_store_credential_reload_successes_total` to increase.
5. Revoke the old credential and confirm writes and checkpoints continue.

Before adoption, a reserved client renews the writer fence with the new
credential and reads it back, so both read and write access must work; a
read-only account is rejected. Queries continue during validation. Then new
storage requests wait briefly while in-flight signed requests finish, the
credential installs, and they resume.

If validation fails (permissions, syntax, authentication, connectivity, or
lost writer ownership), the old credential stays installed, `/readyz` returns
503, the failure counter increases, and an error without credential values is
logged. Fix the file and reload again.

## Release archive

Tagged releases publish a Linux x86-64 archive with an adjacent SHA-256 file.
It contains the executable, failover monitor, starter configuration, systemd
units, license, roadmap, and documentation. CI extracts it, starts the
packaged configuration, probes `/livez`, and checks graceful shutdown. See
[installation](../packaging/README.md).

## Controlled replacement

**User example:** to move the database to another machine, start a server
with the same bucket and prefix but its own local cache directory. Starting it
takes write ownership; an old server that later wakes up cannot publish over
it. Changing client routing is up to you.

1. Confirm the replacement's bucket, prefix, configuration, and credentials.
2. Start it. Recovery promotes a fresh writer incarnation and invalidates both
   mutable-root generations before readiness.
3. Wait for its `/readyz` and check its LSN and capacity metrics.
4. Route clients to it and stop the old process. If the old process is still
   running, its `/readyz` returns 503 and its next publication fails at the
   writer fence.
5. Confirm WAL, checkpoint, and block-object counters advance and logs show no
   publication errors.

If startup fails, fix the reported recovery or object-store error. Promotion
may already have displaced the old writer, so route only to a process whose
`/readyz` succeeds. Retrying an interrupted promotion is safe: it grants no
ownership until startup repeats the conditional root invalidation.

To restore a named backup or a point in retained history, follow
[backup and restore](backup-restore.md). Those offline commands take writer
ownership, so no server may keep using the same prefix.

## Automatic failure detection and promotion

The release includes `libexec/pos3ql/failover-monitor`, an example
configuration, and `pos3ql-failover.service`. Run it on one passive candidate
per durable prefix, with that host's `pos3ql.service` disabled: starting the
database is the promotion. The monitor does not change load balancers or
client addresses; test client routing during deployment qualification.

The monitor probes the primary's `/readyz` with bounded timeouts. After the
configured number of consecutive failures plus one confirmation failure, it
runs its promotion command (an argument vector, not a shell string) exactly
once. A lock directory prevents a second local monitor from promoting. The
unit starts `pos3ql.service`, waits for the candidate's `/readyz`, and stays
active. A failed start or readiness timeout leaves the unit failed for
inspection rather than retrying.

1. Install the same release with the same bucket, prefix, credentials, and
   capacities, but separate local cache storage.
2. Copy `pos3ql-failover.conf.example` to `/etc/pos3ql/pos3ql-failover.conf`;
   set a remotely reachable primary readiness URL and the candidate's
   loopback readiness URL.
3. Confirm `pos3ql.service` is disabled and stopped on the candidate.
4. Enable and start `pos3ql-failover.service`; watch its journal.
5. Route clients only to a process whose `/readyz` returns 200. After a
   promotion, set up a new passive candidate.

Losing connectivity from the monitor to the primary triggers promotion by
design, so use one monitor per prefix on the same network path as client
routing. Object storage decides ownership: promotion rewrites both mutable
roots with the candidate's incarnation before activating its fence. A resumed
old primary then reports 503, and its next publication fails with SQLSTATE
`40001`, even when the storage service derives identical ETags from identical
content.
