# S3-compatible object storage

With `object_store = on`, committed data is saved in object storage. Losing
local caches does not lose that data: a replacement server can recover it from
the configured bucket and prefix. With `object_store = off`, the local journal
is the durable copy instead; losing it loses local-only data.

**User example:** after `COMMIT` succeeds in durable mode, losing the server's
disk should not lose the saved order. If storage publication fails, success
must not be sent. If the final response is lost, the client may not know whether
the commit happened; it must check before retrying an operation that could
charge the customer twice.

pos3ql connects directly to the shared S3-compatible HTTP API. It uses one
implementation for supported endpoints, with no vendor SDK, translation proxy,
or database behavior selected by the endpoint's brand.

An endpoint is supported only after tests prove the operations below behave
correctly. Bucket creation, account administration, and credentials are set up
outside pos3ql; an “S3-compatible” label alone is not enough.

## Concurrent requests and durable commits

**User example:** two customers save orders in different tables. Both can wait
on the same log publication. Reading a cached invoice can continue while that
upload waits; success for either order waits for its own durable log position.

`object_store_get_slots` reserves independent read clients. A selected-column
scan starts a bounded batch of missing columns before waiting, retaining
completed extents across resumption. Atomic operations use a separate block
client and can still wait synchronously; SQL CPU execution remains serial.
A SELECT uses that client from its first routine or state-changing expression
until the statement finishes, including reads after a routine returns. For
example, a function can prepare a query and then read an uncached invoice
without preparing the same query twice. Pure reads keep their concurrent path.

`object_store_put_slots` reserves upload clients and immutable request bodies.
A log batch has two independent objects: transaction bytes and a descriptor.
Those uploads can complete out of order. The commit-head update starts only
after both succeed. Clients retain their transaction outcome until publication
and ordered in-memory completion finish, including when the client disconnects.

`object_store_block_put_slots` reserves separate clients for immutable row and
index output. Table slices, paced merges and secondary indexes retain one operation's bytes while
its uploads overlap; the new file handle stays private until every upload
finishes. Table slices retain their selected bytes while later changes append
another generation. Replacing a table or rewriting its row encodings discards
the old capture. Index sort runs and final index handles remain private until
their uploads finish; changed index inputs cancel abandoned uploads without
waiting for socket readiness. Merge source failures restart the same immutable
pair with cleared output and checksum state.
A separate startup-reserved client and metadata buffer upload the manifest,
the metadata naming the saved files. Its exact bytes survive retries, including a lost response.
Completion installs matching table and index identities and preserves newer
changes. Cleanup keeps every file named by that saved manifest, even when a
later table rewrite has stopped using its indexes.

An explicit SQL CHECKPOINT waits for its requested save while other connections
continue working. Later writes do not extend an earlier command indefinitely;
failures reach the commands waiting for that save. Remaining qualification is in
[the plan](../PLAN.md).

Table output schedules at most the configured number of committed versions,
processes at most 64 per CPU turn, and stops to drain each emitted batch.
Deleting or truncating rows during capture retains deletion markers until the
complete saved-file list installs. For example, deleting an order during its
first checkpoint cannot make the order reappear after that checkpoint.
Cleanup removes only deletion markers covered by the saved file list. Finishing
an older checkpoint must also preserve newer ANALYZE results and the journal
work of an analysis another transaction has not committed.
Metadata capture and other atomic maintenance reads can still wait
synchronously. Rows larger than one block use bounded overflow chains in the
same generation; reading such a row requires its complete chain.


| Setting | Reservation or rejection |
|---|---|
| `object_store_get_slots` | Fixed read clients; at least 1 in object-store mode. |
| `object_store_put_slots` | Fixed journal upload clients; at least 2 in object-store mode. |
| `object_store_block_put_slots` | Fixed physical-block upload clients; at least 1 in object-store mode. Each reserves one 256 KiB body. |
| `wal_upload_buffer_bytes` | Immutable body bytes per journal upload slot, plus the engine's captured log batch; must be at least `wal_buffer_bytes`. |
| Credential validation | One conditional upload client with a 128-byte body when `object_store_credentials_file` is configured. The candidate must also read back the result. |
| Operational ownership check | One fixed client and one receipt slot per `operations_max_connections` when the operational listener and object storage are enabled. Shares the explicit TLS budget. |
| `checkpoint_manifest_bytes` | Bytes in each of three fixed buffers: metadata scratch, retained manifest, and manifest upload body. |
| `checkpoint_write_entries` | Committed versions retained by one table output schedule; SQLSTATE 54000 names exhaustion before publication. |
| `commit_queue_slots` | Complete transaction and session-context buffers retained through asynchronous publication; at least 1 in object-store mode. |

The development fixture selects 4 read slots, 4 journal upload slots, 4 physical
upload slots and 4 commit queue slots. Size their startup memory reservations explicitly for a deployment.
An exhausted commit queue or log buffer returns SQLSTATE `54000`, naming
`commit_queue_slots` or `wal_upload_buffer_bytes`. It never grows a pool.

TLS has a separate startup ceiling. `tls_pool_bytes` supplies the object-store
base; startup adds 128 KiB for each of the two transports per subscription
worker, plus 128 KiB per server connection when server TLS is enabled. Choose
the base for the configured object-store request concurrency. These reservations
are fixed before the server accepts connections.

Nonblocking reads have a 30-second deadline. Each upload attempt has a
30-second deadline, with at most three attempts for retryable failures and
200 ms then 400 ms backoff. A failed conditional create compares the entire
stored object, including its length, before accepting an uncertain prior
success. A different mutable root remains a writer conflict.

Login triggers use the fixed commit queue. If a login trigger records an audit
row, authentication waits for that row to become durable while the server can
handle other connections. Waking the login again does not repeat its triggers.

A procedure's internal `COMMIT` and standalone catalog commands retain explicit
synchronous completion boundaries: their call stack cannot be replayed after a
commit. During credential rotation they finish accepted journal work with the
installed credentials; replacement waits for active requests to finish. Ordinary
client commits and streaming subscription apply use the fixed commit queue.

Saved query results are collected in one scan within the fixed statement arena.
They do not run expressions a second time to discover the result size. For
example, a saved query that calls a function to prepare a statement must prepare
it once, even when another part of the query reads uncached data. Sorting by
that query's output label or position reuses the saved value, following
[PostgreSQL's ORDER BY rules](https://www.postgresql.org/docs/18/sql-select.html#SQL-ORDERBY).

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
database within it. Object keys and list prefixes are encoded once at the S3
client boundary; storage and database code never inspect endpoint brands.

ETags are opaque generation tokens. pos3ql must not assume that an ETag is an
MD5 digest or infer its shape from the provider. Conditional failure, missing
objects, throttling, transient transport failure, authentication failure, and
malformed responses remain distinct typed outcomes.

### Preventing an old server from overwriting a replacement

The `writer-fence` object records which process may write. For example, after
you replace a failed primary, that primary must not overwrite newer commits if
it later reconnects. Compare-and-swap (CAS) means updating a reference only if
its stored generation still matches the one read earlier.

Promotion first marks ownership as changing, updates the commit head and
checkpoint manifest with the replacement's identity, then activates that
identity. The commit head names recoverable commits; the manifest names the
checkpoint's saved objects. A delayed old request either lands before the
replacement updates its root and is preserved, or fails because the generation
has changed. Uploaded objects without a successful root update cannot become
confirmed commits. [Durable formats](durable-format.md#format-details-for-implementers)
specifies empty roots, writer identities, conflict handling, and upgrade rules.

After an ownership check detects a replacement, new SQL work is rejected with
SQLSTATE `40001` before another commit enters the journal; rollback remains
available. An operation already awaiting publication has a different outcome:
if durability cannot be confirmed, the server sends a terminal `08007` error,
flushes it before closing the connection, and retains the transaction for
recovery. Reconnect and check the order before trying to save it again.

Profile v1 accepts S3's 1,024-byte object-key limit, opaque continuation tokens
up to 1,024 bytes, and strong quoted ETags up to 80 bytes. Exceeding a fixed
profile bound fails before I/O or while decoding; it never truncates a key,
token, validator, or signed request.

This subset is the **pos3ql S3 compatibility profile v1**. It selects existing
S3-compatible protocol behavior; it does not define a pos3ql wire protocol.
The profile freezes:

- HTTP methods, request targets, query parameters, and required headers;
- canonical-request construction and Signature Version 4 inputs;
- virtual-hosted and path-style addressing;
- successful response headers and XML list bodies;
- structured XML errors, HTTP status mapping, and retry classification;
- conditional-write, range, pagination, deletion, and consistency semantics;
- configuration fields and their meaning at the object-client boundary.

Changing any item requires an explicit profile-version change, compatibility
analysis, updated fixtures, and qualification against every required endpoint.
Implementation refactors must not change the profile.

## Portability invariants

- The provider-neutral `block store` is an internal database interface, not a
  new network protocol.
- The production binary talks directly to a qualified S3-compatible endpoint.
- One client implementation and configuration model serves every qualified
  endpoint. There are no vendor adapters or fallback protocols.
- Object-store differences are admitted only when they preserve the required
  S3-compatible semantics. Unsupported behavior fails at startup or at the
  request boundary.
- Bucket administration, identity provisioning, credential discovery, and
  other provider control-plane operations remain outside pos3ql.

## Transport and authentication

**User example:** when temporary storage credentials expire, install their
replacement while both credentials still work, then reload. A rejected
replacement keeps the installed credential but makes readiness fail so routing
can stop until the problem is corrected.

The direct client uses HTTP over TLS, S3 virtual-hosted or path-style bucket
addressing, and S3 Signature Version 4 as the shared request-signing
protocol. Configuration supplies an endpoint, region/signing scope, bucket,
and either direct credentials or an owner-only `object_store_credentials_file`.
The file contains strict `access_key`, `secret_key`, and optional
`session_token` entries. Credential discovery through a vendor SDK, instance
metadata service, or provider control plane is outside the engine.

Credential reload validates the candidate through a conditional writer-fence
renewal before installing it. Failure retains the installed credential and
marks readiness false. Follow the atomic replacement and overlapping-validity
procedure in [operations](operations.md#object-store-credential-rotation).

Request slots, signing state, retry state, response headers, XML list/error
decoding, and page buffers are fixed at startup. Retries are limited to typed
retryable outcomes and preserve conditional-request identity; they are not a
fallback to different semantics or another provider API.

Checkpoint maintenance uses the same provider-neutral list and delete contract.
Cold commit-chain replay, live-block retention, SST-pair merge scratch, and
garbage deletion batches have independent startup capacities. A garbage batch
is a pacing unit, not a namespace-size limit: automatic checkpoints continue it
between statements, while an explicit checkpoint returns only after all listed
obsolete objects are deleted. Live-set or recovery-chain exhaustion fails with
the corresponding configuration name and never weakens retention.

Backups and independent-prefix exports use the same protocol and fixed
capacities. Backup roots extend the live set and commit-retention floor;
capacity exhaustion stops cleanup before deletion. Export streams immutable
objects through bounded ranged GETs and conditional PUTs, with pending markers
that prevent startup from observing an incomplete destination. The full
retention, retry, and publication contract is in [backup and restore](backup-restore.md).

## Qualification

CI tests MinIO and SeaweedFS as two independent S3 implementations, plus a
deterministic fixture for exact requests and failures. Those services run on
the test machine. The same executable can test a separately operated endpoint;
that deployment evidence is still required by [the plan](../PLAN.md).

The checks are:

1. Golden-wire tests compare exact requests, canonical signing inputs, parsed
   responses, pagination, error mapping, TLS transport, and retry behavior with
   checked-in profile-v1 fixtures.
2. The black-box durability suite runs the production client against two
   independently implemented, pinned S3-compatible servers. It proves
   conditional create and compare-and-swap, opaque ETag handling, full and
   ranged reads, prefix listing, idempotent deletion, signing, commit-batch
   recovery, delta-checkpoint carry-forward, and cold start with both local
   caches absent. The deterministic profile fixture forces multi-page listing
   through the same qualification executable.
3. The identical qualification executable can also target independently operated
   S3-compatible endpoints when credentials are available. It contains no
   provider selection or provider-specific expectations. This optional run is
   not evidence supplied by the local MinIO and SeaweedFS CI jobs; representative
   independent-service qualification remains on the roadmap.

For example:

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

CI architecture guards enforce the profile request and authentication boundary
and reject vendor SDK dependencies, provider-named branches or configuration,
and fallbacks that change protocol semantics.

Passing that suite is the admission criterion. Marketing an API as
"S3-compatible" is not sufficient. A fixture change cannot be used merely to
make a behavior change pass: it is a reviewed compatibility-profile change.
No S3 request, XML, signing, bucket, region, credential, or endpoint type
escapes into commit publication, checkpointing, caching, SQL, or other database
code. The in-process deterministic simulator remains a test implementation of
the internal block-store interface; it is not a network service.
