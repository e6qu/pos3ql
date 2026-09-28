# Operations

pos3ql can expose a separate HTTP listener for process probes, Prometheus
metrics, and startup capacity. It is disabled by default. Enable it on a
private interface:

```text
operations_listen_addr = 127.0.0.1:9187
operations_max_connections = 4
log_format = json
```

The listener is plaintext and unauthenticated. Do not publish it directly to
an untrusted network. Terminate TLS and apply access control in the surrounding
platform when remote access is required. Its connection slots and 2 KiB
request and 16 KiB response buffers are reserved at startup. Operational
requests therefore remain independent of `max_connections` and cannot grow
runtime memory.

## HTTP endpoints

| Path | Success | Meaning |
|---|---:|---|
| `/livez` or `/healthz` | 200 | The event loop accepted and answered the request. |
| `/readyz` | 200 | Startup completed, durable progress is healthy, and the process still owns the writer fence. |
| `/readyz` | 503 | WAL or checkpoint progress failed, the object store is unavailable, or another process owns the writer fence. |
| `/metrics` | 200 | Prometheus text metrics for current readiness, connections, WAL, row memory, checkpoint pressure, cache traffic, and immutable-block object requests. |
| `/capacity` | 200 | JSON containing current use and startup limits for memory, connections, WAL, the row heap, caches, catalogs, prepared transactions, replication, and whether credential rotation is configured. |

Only `GET` with HTTP/1.0 or HTTP/1.1 is accepted. Unknown paths and methods
fail explicitly. Responses disable caching and close the connection.

In durable mode, `/readyz` and `/metrics` read and validate the writer fence.
This detects a displaced idle process before it can continue serving routed
reads. The request uses the existing fixed object client and allocates no
runtime memory, but it can wait on object-store latency. `/livez` remains a
local event-loop probe for separating process failure from durable-tier
failure. Readiness also turns false when commit publication or checkpoint work
fails and returns true after the pending durable work succeeds.

## Metrics and alerts

Scrape `/metrics` from the private operational network. Useful first alerts
are:

- `pos3ql_up == 0`: the process or operational listener is unreachable.
- `pos3ql_ready == 0`: stop routing new work and inspect publication errors.
- `pos3ql_object_store_credential_reload_failures_total` increases: correct
  the candidate credential file and reload it before routing resumes.
- PostgreSQL connections approaching `pos3ql_postgres_connection_capacity`:
  raise the startup limit or reduce client concurrency.
- WAL or row-heap use approaching its capacity: inspect checkpoint progress
  and object-store latency before the named capacity is exhausted.
- Sustained object read latency, cache misses, or prefetch saturation: compare
  the working set with the configured cache and object-read slot capacities.

Counters reset on process start. Block-object request counters cover immutable
SST traffic through pos3ql's provider-neutral cache stack. They exclude root,
commit-batch, LIST, DELETE, and storage-service-internal requests.

## Logs

`log_format = text` retains human-readable diagnostics. `log_format = json`
emits one JSON object per line with `timestamp_unix_ms`, `level`, `event`, and
`message`. Runtime formatting uses fixed stack buffers after memory freezes.
Database credentials and object-store secret material are not logged.

## Object-store credential rotation

Prefer `object_store_credentials_file` over inline credential settings. The
file must be a regular UTF-8 file no larger than 4 KiB and must grant no group
or other permissions. Its format is:

```text
access_key = example-access
secret_key = example-secret
session_token = optional-temporary-token
```

Rotate without interrupting durable work:

1. Grant the replacement credential access to the configured bucket and
   prefix while the current credential remains valid.
2. Write the complete replacement file beside the configured path with mode
   `0600`, then rename it over the configured path atomically.
3. Send `SIGHUP` to the server or execute `SELECT pg_reload_conf()`.
4. Wait for `/readyz` to return 200 and for
   `pos3ql_object_store_credential_reload_successes_total` to increase.
5. Revoke the prior credential and confirm durable writes and checkpoints
   continue.

The candidate is parsed without runtime allocation and tested by a conditional
writer-fence renewal before any client adopts it. Invalid permissions, syntax,
authentication, connectivity, or lost writer ownership retain the installed
credential, make readiness return 503, increment the failure counter, and emit
an error without credential values. Correct the file and reload again.

## Release archive

Tagged releases publish a versioned Linux x86-64 archive and adjacent SHA-256
file. The archive contains the executable, starter configuration, systemd unit,
license, roadmap, and operator documentation. CI extracts the same archive,
runs its executable, starts it from the packaged configuration, probes
`/livez`, and verifies graceful shutdown. Installation steps are in
[`packaging/README.md`](../packaging/README.md).

## Controlled replacement

1. Confirm the replacement uses the intended bucket and prefix, configuration,
   and durable credentials.
2. Start the replacement. Startup recovery promotes its fresh writer
   incarnation and invalidates both mutable-root generations before readiness.
3. Wait for the replacement's `/readyz` response and verify its LSN and
   capacity metrics.
4. Route clients to the replacement and stop the prior process. If the prior
   process was still alive, its readiness probe returns 503 and its next
   durable publication fails through the writer fence.
5. Confirm WAL, checkpoint, and block-object request counters continue advancing and
   that no publication errors remain in logs.

If replacement startup fails, leave clients on the current process and resolve
the reported recovery or object-store error. An interrupted promotion is safe
to retry: transition state grants no writer ownership, and startup repeats the
conditional root invalidation before activation.

For recovery from a named backup or a point in retained history, follow
[backup and restore](backup-restore.md). Those commands are offline operations
and promote their own writer incarnation, so no server may continue using the
same prefix.

## Automatic failure detection and promotion

The release archive includes `libexec/pos3ql/failover-monitor`, an example
configuration, and `pos3ql-failover.service`. Run the service on one passive
candidate for a durable object prefix. Keep that host's `pos3ql.service`
disabled because starting the database is the promotion action.

The monitor probes the primary's `/readyz` endpoint with bounded connect and
request timeouts. It requires the configured number of consecutive failures
and one final confirmation failure, then runs its argument-vector promotion
command exactly once. It does not evaluate a shell command. A lock directory
prevents a second local monitor from promoting concurrently. The systemd unit
starts `pos3ql.service`, waits for the candidate's `/readyz`, and remains
active after success. A failed start or readiness timeout leaves the unit
failed for operator inspection instead of retrying promotion indefinitely.

Configure the passive candidate:

1. Install the same release and configure the same bucket, prefix, durable
   credentials, and capacity envelope with separate local cache storage.
2. Copy `pos3ql-failover.conf.example` to
   `/etc/pos3ql/pos3ql-failover.conf`. Set a remotely reachable primary
   readiness URL and the candidate's loopback readiness URL.
3. Confirm `pos3ql.service` is disabled and stopped on the passive host.
4. Enable and start `pos3ql-failover.service`, then monitor its journal.
5. Route clients only to a process whose `/readyz` returns 200. After
   promotion, provision and arm a new passive candidate.

A loss of connectivity from the monitor to the primary intentionally triggers
promotion after the threshold. Use one monitor authority per prefix and place
it on the same failure-observation path as client routing. Object storage
remains the ownership authority: promotion rewrites both mutable roots with the
candidate's process incarnation before activating its writer fence. A resumed
primary therefore reports 503 and its next durable publication fails with
SQLSTATE `40001`, including when the storage service derives identical ETags
from identical content.
