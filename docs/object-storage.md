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
