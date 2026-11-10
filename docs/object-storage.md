# S3-compatible object storage

With `object_store = on` (durable mode), committed data is saved in object
storage. RAM and disk are caches: a replacement server can recover everything
from the configured bucket and prefix. With `object_store = off` (local mode),
the local journal file is the durable copy; losing that disk loses the data.

pos3ql talks directly to the S3 HTTP API with one built-in client. There is no
vendor SDK, translation proxy, or behavior chosen by the provider's brand.
Bucket creation, accounts and credential issuing happen outside pos3ql.
An endpoint is supported only after it passes the
[qualification suite](#qualification); an "S3-compatible" label is not enough.

[Architecture](architecture.md#writes-and-the-success-response) describes the
commit path, checkpoints and caches in detail.

## What a successful commit means

| Mode | `COMMIT` succeeds after | Losing the server's disk |
|---|---|---|
| `object_store = off` | The local journal is synced to disk. | Loses local-only data. |
| `object_store = on` | The transaction's log objects are uploaded and the commit head is conditionally updated. | Loses nothing committed; recovery reads object storage. |

**User example:** after `COMMIT` succeeds in durable mode, losing the server's
disk does not lose the saved order.

Failure cases:

- **Object store unavailable during commit.** Uploads are retried (see
  [timeouts and retries](#timeouts-and-retries)). Success is never sent unless
  publication completes. If publication of a staged commit fails, durability
  cannot be confirmed: the server sends the fatal error SQLSTATE `08007`
  ("commit outcome is unknown; reconnect before checking it"), flushes it,
  closes the connection and keeps the transaction for recovery. The commit may
  or may not have happened.
- **Lost success reply.** The client cannot tell whether the commit happened.
  Reconnect and check before retrying anything that must not run twice, such as
  charging a customer.
- **Installation fails after the commit is durable.** The instance stops
  accepting queries and checkpoints, keeps the original error and does not undo
  the durable change. Further work returns `08007` until restart and recovery.
  This applies in both modes.
- **Replaced by another server.** See
  [preventing an old server from overwriting a replacement](#preventing-an-old-server-from-overwriting-a-replacement).

A commit holds its locks, transaction identity and serializable checks until
its rows are installed in memory, so a competing change cannot slip in between
an order and its invoice. A client that disconnects while its commit is being
published does not cancel it. Several connections' commits share one upload
batch.

## Required S3-compatible subset

| S3 operation | Required behavior |
|---|---|
| `PutObject` with `If-None-Match: *` | Create immutable data or fail without replacing an existing object. |
| Conditional `PutObject` with `If-Match` | Replace precisely the named generation or fail. |
| `GetObject` | Return the requested object and its opaque, quoted ETag. |
| Ranged `GetObject` | Honor inclusive single byte ranges. |
| `ListObjectsV2` | Return every key under a prefix across continuation pages. |
| `DeleteObject` | Delete one key; deleting an absent key is idempotent. |

The bucket is the durable namespace. `object_store_prefix` isolates one pos3ql
instance, including all its SQL databases. Keys and list prefixes are encoded
once at the client boundary.

An ETag is an opaque generation token: pos3ql never assumes it is an MD5 digest.
Conditional failure, missing object, throttling, transient transport failure,
authentication failure and malformed response are distinct outcomes.

**Profile limits (v1).** Object keys up to 1,024 bytes (the S3 limit),
continuation tokens up to 1,024 bytes, strong quoted ETags up to 80 bytes.
Exceeding a bound fails before I/O or while decoding; nothing is truncated.

This subset is the **pos3ql S3 compatibility profile v1**. It selects existing
S3 behavior; it is not a new wire protocol. It freezes:

- HTTP methods, request targets, query parameters and required headers;
- canonical-request construction and Signature Version 4 inputs;
- virtual-hosted and path-style addressing;
- successful response headers and XML list bodies;
- structured XML errors, HTTP status mapping and retry classification;
- conditional-write, range, pagination, deletion and consistency semantics;
- configuration fields and their meaning at the client boundary.

Changing any item requires a new profile version, updated fixtures and
qualification against every required endpoint.

### Preventing an old server from overwriting a replacement

The `writer-fence` object records which process may write. For example, after
you replace a failed primary, that primary must not overwrite newer commits if
it reconnects. Compare-and-swap (CAS) means updating an object only if its
stored generation still matches the one read earlier.

Promotion marks ownership as changing, updates the commit head and checkpoint
manifest with the replacement's identity, then activates it. The commit head
names recoverable commits; the manifest names a checkpoint's saved objects.
A delayed request from the old server either lands before the replacement
updates its root (and is preserved) or fails because the generation changed.
Uploaded objects without a successful root update never become confirmed
commits. [Durable formats](durable-format.md#format-details-for-implementers)
specify the roots and conflict rules.

After a server detects that it was replaced, new SQL work fails with SQLSTATE
`40001` before entering the journal; rollback still works. A commit already
being published gets the terminal `08007` error described above.

## Timeouts and retries

| Behavior | Value |
|---|---|
| Read deadline | 30 seconds per request. |
| Upload deadline | 30 seconds per attempt. |
| Attempts | At most 3 for retryable failures, with 200 ms then 400 ms backoff. |
| `object_store_hedge_after_ms` | If nonzero, a read still pending after this many milliseconds is also sent on a spare read slot; the first response wins. Default `0` (off). |

Retries apply only to typed retryable outcomes and resend the same conditional
request; they never switch to different semantics. If a conditional create
fails because the object exists, pos3ql compares the entire stored object,
including its length, before accepting it as an earlier attempt's success.
A different mutable root remains a writer conflict. Retrying a `DELETE` whose
response was lost is safe. Background cleanup that fails keeps its position
and retries on the normal one-second maintenance cadence.

## Concurrency and fixed capacity

All request clients and buffers are reserved at startup; exhaustion is a named
error, never a growing pool.

| Setting | Reservation or rule |
|---|---|
| `object_store_get_slots` | Read clients; at least 1 in durable mode. |
| `object_store_put_slots` | Journal upload clients; at least 2 in durable mode. A commit's transaction bytes and descriptor upload in parallel; the commit head updates after both succeed. |
| `object_store_block_put_slots` | Row and index file upload clients; at least 1 in durable mode. Each reserves one 256 KiB body. |
| `wal_upload_buffer_bytes` | Immutable body bytes per journal upload slot; must be at least `wal_buffer_bytes`. |
| `commit_queue_slots` | Commits retained through asynchronous publication; at least 1 in durable mode. |
| `checkpoint_manifest_bytes` | Size of each of three manifest buffers (scratch, retained, upload body). |
| `checkpoint_write_entries` | Committed versions in one table output schedule. |
| `checkpoint_garbage_batch_objects` | Objects deleted per cleanup batch. A pacing unit, not a namespace limit; file-cleanup listings read up to `min(value, 1000)` keys per page. |
| `object_store_head_bytes`, `object_store_response_bytes` | Response header and body buffers per client. |
| `tls_pool_bytes` | Object-store TLS base. Startup adds 128 KiB for each of two transports per subscription worker, and 128 KiB per server connection when server TLS is on. |
| `disk_cache_bytes` | Disk-cache file size, plus RAM for its slot directory, index and one 256 KiB scratch buffer. |

Separate startup-reserved clients serve atomic metadata operations, manifest
upload, credential validation (one client, 128-byte body, when
`object_store_credentials_file` is set), the operational ownership check (one
client plus one receipt slot per `operations_max_connections`), journal cleanup
and saved-file cleanup. Disabled cache tiers reserve no memory; with
`object_store = off`, no object-store cache is built.

An exhausted commit queue, log buffer or `checkpoint_write_entries` returns
SQLSTATE `54000` naming the setting. Exhausted live-set or recovery-chain
capacity fails with the setting's name and never weakens retention. The
development fixture uses 4 slots each for reads, journal uploads, file uploads
and the commit queue; size them explicitly for a deployment.

While a request waits on the network, the SQL worker serves other connections.
For example, reading a cached invoice continues while another connection's
commit uploads, and while cleanup lists or deletes old files. Two behaviors are
worth knowing:

- Once the first bytes of a large result have reached the client, its
  remaining reads finish synchronously, because a sent prefix cannot be
  withdrawn. Rows the client has not yet read queue in the result spool and
  never hold the worker; resumable output is
  [planned work](../PLAN.md#pause-and-resume-every-query-path).
- Setting both caches to zero does not evict rows already in memory: an
  `INSERT` followed by `SELECT` can return the row without a download. To test
  cold reads, make the writes durable, then restart in durable mode with an
  empty `data_dir`.

## Transport and credentials

The client uses HTTP over TLS (`object_store_tls = on`), virtual-hosted or
path-style addressing (`object_store_addressing`), and S3 Signature Version 4.
Configure:

- `object_store_endpoint`, `object_store_region` (signing scope),
  `object_store_bucket`, `object_store_prefix`;
- `object_store_tls_ca_file` for a private certificate authority;
- credentials, either inline (`object_store_access_key`,
  `object_store_secret_key`, optional `object_store_session_token`) or,
  preferably, an owner-only `object_store_credentials_file` with strict
  `access_key`, `secret_key` and optional `session_token` entries.

pos3ql does not discover credentials through a vendor SDK, instance metadata
service or provider control plane.

**User example:** when temporary credentials expire, install their replacement
while both still work, then reload. Reload validates the candidate with a
conditional writer-fence renewal before installing it. A rejected candidate
leaves the installed credential in place and makes readiness fail, so routing
can stop until the problem is fixed. In-flight repeatable requests restart from
the same position with the new credential. See
[operations](operations.md#object-store-credential-rotation) for the procedure.

Backups and exports use the same client and capacities; see
[backup and restore](backup-restore.md).

## Qualification

CI runs two independent S3 implementations, MinIO and SeaweedFS, pinned by
image digest and running on the test machine, plus a deterministic fixture for
exact requests and failures:

1. **Golden-wire tests** compare exact requests, signing inputs, parsed
   responses, pagination, error mapping, TLS transport and retries with
   checked-in profile-v1 fixtures. A fixture change is a reviewed profile
   change, not a way to make a behavior change pass.
2. **Black-box durability suite** runs the production client against both
   servers: conditional create and compare-and-swap, opaque ETags, full and
   ranged reads, prefix listing, idempotent deletion, signing, commit-batch
   recovery, delta-checkpoint carry-forward, and cold start with both local
   caches absent. The deterministic fixture forces multi-page listing.
3. **External endpoints.** The same executable, with no provider-specific
   expectations, can target a separately operated endpoint:

```sh
POS3QL_OBJECT_STORE_ENDPOINT=objects.example:443 \
POS3QL_OBJECT_STORE_BUCKET=pos3ql-qualification \
POS3QL_OBJECT_STORE_REGION=us-east-1 \
POS3QL_OBJECT_STORE_ACCESS_KEY=... \
POS3QL_OBJECT_STORE_SECRET_KEY=... \
POS3QL_OBJECT_STORE_TLS=on \
cargo test --locked --test object_store_it -- --test-threads=1
```

Set `POS3QL_OBJECT_STORE_SESSION_TOKEN` for temporary credentials,
`POS3QL_OBJECT_STORE_ADDRESSING=virtual_hosted` when required, and
`POS3QL_OBJECT_STORE_TLS_CA_FILE` for a private certificate authority.

The CI services share a host with the tests, so they prove portability across
implementations, not behavior of a separately operated service. That evidence
is part of [qualifying production deployment](../PLAN.md#qualify-production-deployment).

CI architecture guards reject vendor SDK dependencies, provider-named branches
or settings, and fallbacks that change protocol semantics. No S3 request, XML,
signing, bucket or credential type reaches commit publication, checkpointing,
caching or SQL code. The in-process simulator (`object_store = sim`) is a test
implementation of the internal block-store interface, not a network service.
