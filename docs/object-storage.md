# S3-compatible object storage

pos3ql treats object storage as the only durable tier. The production protocol
boundary is the common S3-compatible HTTP API implemented by independently
built object stores. The engine connects to that API directly. It does not use
a vendor SDK, intermediary storage service, translation proxy, or behavior
selected by an endpoint name.

This is a protocol-compatibility claim, not a claim that every object-storage
product has identical control-plane features. An endpoint is supported only
when the qualification suite proves the required data-plane semantics.

## Required S3-compatible subset

| S3 operation | Required semantic |
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

The direct client uses HTTP over TLS, S3 virtual-hosted or path-style bucket
addressing, and S3 Signature Version 4 as the shared request-signing
protocol. Configuration supplies an endpoint, region/signing scope, bucket,
access key, secret key, and optional temporary session token. Credential
discovery through a vendor SDK, instance metadata service, or provider control
plane is outside the engine.

Request slots, signing state, retry state, response headers, XML list/error
decoding, and page buffers are fixed at startup. Retries are limited to typed
retryable outcomes and preserve conditional-request identity; they are not a
fallback to different semantics or another provider API.

## Qualification

Required CI has three independent layers:

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
3. The identical qualification executable can target independently operated
   S3-compatible endpoints when credentials are available. It contains no
   provider selection or provider-specific expectations.

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

CI also contains architecture guards that reject the old custom request path
and authentication scheme, vendor SDK dependencies, provider-named branches or
configuration, and any fallback that changes protocol semantics.

Passing that suite is the admission criterion. Marketing an API as
"S3-compatible" is not sufficient. A fixture change cannot be used merely to
make a behavior change pass: it is a reviewed compatibility-profile change.
No S3 request, XML, signing, bucket, region, credential, or endpoint type
escapes into commit publication, checkpointing, caching, SQL, or other database
code. The in-process deterministic simulator remains a test implementation of
the internal block-store interface; it is not a network service.
