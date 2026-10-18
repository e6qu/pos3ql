# Durable format compatibility

Use this guide when upgrading a database that already has saved objects.
New binaries must recognize those objects before reading them, and older
binaries may not recognize newly written formats. The table below states the
actual supported readers and writers.

**User example:** a current server can open a checkpoint written in manifest
v13 or v14 and write v15 at its next checkpoint. Do not assume you can then downgrade
to a binary that cannot read v15. No incompatible offline migration is currently
available, and unknown formats stop startup explicitly.

Object storage is the durable tier. Immutable objects may outlive the binary
that wrote them, so every root and row SST carries a format identity that is
checked before its bytes are interpreted. Unknown identities stop startup;
they never select a guessed decoder.

## Current compatibility matrix

| Object | Identity | Read | Write | Migration |
|---|---|---:|---:|---|
| Checkpoint manifest | `pos3ql-manifest-v13` | yes | no | The next successful checkpoint publishes v15. |
| Checkpoint manifest | `pos3ql-manifest-v14` | yes | no | The next successful checkpoint publishes v15. |
| Checkpoint manifest | `pos3ql-manifest-v15` | yes | yes | Current enum member identities and legacy label bindings. |
| Empty checkpoint root | `pos3ql-empty-manifest-v1` | yes | yes | Fences an as-yet unpublished manifest root. |
| Commit head | `pos3ql-commit-head-v1` | yes | yes | Names the immutable commit chain and the publishing process incarnation. |
| Empty commit root | `pos3ql-empty-commit-head-v1` | yes | yes | Fences an as-yet unpublished commit root. |
| Writer fence | `pos3ql-writer-fence-v1` | yes | yes | Serializes promotion and mutable-root publication. |
| Backup completion | `pos3ql-backup-v1` | yes | no | Named-point and LSN restore remain readable; no backup creation timestamp is available. |
| Backup completion | `pos3ql-backup-v2` | yes | yes | Checksums both roots and anchors timestamp recovery after backup creation. |
| Point-in-time restore marker | `pos3ql-point-in-time-restore-v1` | yes | yes | Binds the backup, manifest checksum, resolved LSN, and commit head during root replacement. |
| Backup export marker | `pos3ql-export-v1` | yes | yes | Pending and completion records make an independent-prefix copy restartable. |
| Published row SST | `v2` | yes | no | Compatible generations remain readable and are replaced when sliced or merged. |
| Published row SST | `v3` | yes | no | Compatible packed PAX generations remain readable. |
| Published row SST | `v4` | yes | yes | Current packed format: PAX full slices and row-packed deltas. |

## Why the storage layout matters

**User example:** an orders table has an ID, status, and a large description.
A query that needs only IDs and statuses can read those column extents from
a full PAX slice without fetching the description solely to decode each row.
Small updates use packed row groups instead of writing a separate descriptor
and column container for every small group.

Packing shares objects across useful data ranges; ranged reads can fetch the
needed extent. These mechanisms are implemented. Their effect on latency,
downloaded bytes, and rewritten bytes must still be measured for representative
workloads. They do not promise that every query reads only projected columns:
filters and exact result checks can need more data.

## Format details for implementers

The manifest header versions the catalog and root record grammar. Each `dsst`
manifest reference separately names its row SST format. `v2` uses direct
content-addressed data-block references. `v3` uses packed references to PAX
descriptors and independently verified column extents. `v4` retains that PAX
representation for full slices and also admits compressed canonical row groups
in verified packed extents. Small deltas use row groups so they do not pay a
separate PAX descriptor and column-container write per group. The in-memory
handle retains this identity as `RowSstFormat`; index traversal cannot infer an
index-entry grammar from block contents or collapse the identity into an
unrelated flag.

A named backup stores an exact manifest and commit-head image, then publishes
its completion record last. Version 2 adds the backup creation timestamp to
the manifest LSN and CRC32C checksums for both roots. Restore rejects a missing,
unknown, or mismatched completion record. The backup manifest itself is the
retention pin, so an interrupted creation cannot expose a restorable name or
lose blocks that its partial state may reference.

The transaction identity counter has one guarded owner shared by SQL and
maintenance. For example, two connections starting transactions cannot receive
the same identity; a full active registry rejects the new transaction before
changing its session. The existing `latest_transaction_id` manifest field and
journal commit identities preserve the counter through recovery. Exhaustion
of the 32-bit internal identity space returns `54000` before reuse. These
changes preserve the existing encodings and startup reservation sizes.

Current commit boundary records append PostgreSQL-epoch commit microseconds to
the transaction identity fields. Readers retain the empty, identity-only, and
identity-plus-assignment legacy forms. LSN recovery can use legacy records;
timestamp recovery rejects a legacy boundary whose time cannot be known.
Recovery may write an immutable prefix of a multi-transaction commit batch.
Its identity is the CRC32C of the exact prefix and its descriptor preserves the
original predecessor, so the published head ends only at a validated whole
transaction.

Every process creates a random 128-bit writer incarnation at startup. It first
CAS-publishes a transitioning `pos3ql-writer-fence-v1` record, conditionally
rewrites both mutable roots with its process token, and then CASes the fence to
active. Missing roots receive token-bearing empty identities. Existing
manifest and commit-head roots are retagged, so their strong ETags change even
when the provider derives an ETag from object content. Commit and checkpoint
publication use the root ETag loaded by that process; after a conflict they
must prove that their process token still owns the active fence before adopting
or retrying a write. A successor changes the fence before touching either root,
then advances both root ETags before it can become active. The final active CAS
is the ownership-transfer point; a failed or starved transition grants no new
owner. Thus a request begun before promotion either lands and is preserved by
the successor's rewrite, or arrives later with a stale ETag and fails.
An interrupted transition is taken over and completed by the next
startup.

Commit-head v1 readers accept both legacy 64-bit writer labels and current
128-bit process tokens. A prefix without `writer-fence` is promoted in place.
Its manifest and commit head gain the new writer identity; their referenced
database state is preserved, but the root bytes and ETags change. Binaries that
predate writer fencing must be stopped before this first promotion and must not
subsequently open the prefix, because they do not participate in the fence.
The current manifest writer line is 16 bytes wider than its legacy form and is
charged to the existing `checkpoint_manifest_bytes` reservation. The mutable
root promotion scratch adds one fixed 128-byte startup allocation; publication
does not allocate after the runtime memory freeze.

An independent-prefix copy publishes `pos3ql-export-v1` as a pending marker
before copying immutable objects. A retry must match the backup name and root
checksums in that marker. Startup refuses the destination until both live roots
and the destination local-cache reset are complete. An `export-complete` record
with the same body is published before `export-pending` is removed, so a lost
final response can be adopted while the published roots still match, without
treating the populated prefix as a new export destination.

Block headers also carry a typed block identity. A row SST format defines the
allowed index-entry shape and block types together. A recognized block type in
the wrong row SST format is corruption rather than an alternate decoding path.

Journal records version field-width changes with distinct kind bytes. Current
writers use publication kinds 139/140, trigger kind 141, routine kind 142,
composite kind 143, and view kinds 144/145 for wide column sets and 16-bit
counts. Readers retain the preceding publication, trigger, routine, composite,
and view kinds. Stored-query dependencies use marker `0xfe` for the complete
column set and retain the `0xff` and legacy readers. Table statistics use the
v4 marker 253 for 16-bit counts and ordinals while retaining v3 marker 254.
Manifest v14 keeps its text grammar: sparse column sets parse old one-word
values, and wide composite fields stream into the configured manifest buffer.

Manifest v14 also fixes table constraints at 64 entries per modeled kind and
domain checks at 64 entries. Their journal counts are unsigned bytes, while
their positions use a 64-entry stride in synthesized `pg_constraint` and
referential-trigger OIDs. The 65th entry is rejected with SQLSTATE `54000`
before catalog mutation. Raising this boundary would renumber durable catalog
identities, so it requires an explicit format and OID migration rather than a
constant change.

### Enum member identities

An enum member's position in its catalog member list is its stable identity.
Adding a member appends to that list, even when its displayed sort position is
BEFORE another member. Renaming or renumbering never changes an existing member's
identity. Row values keep this identity separately from their cached label and
sort position. Catalog-aware reads select the current label and sort position.

Manifest v15 adds an optional legacy label after each enum member's sort bits.
Older manifests and enum journal kinds 23/147 initialize this binding from their
labels. Journal kind 149 writes the current label, sort bits, and an optional
legacy label for each member; it retains the u32 member count. Existing legacy
bindings survive later label changes. No missing identity is guessed.

A row enum's u32 label-length high bit marks an additional u32 member identity
before its cached label. A clear high bit retains the legacy decoder. Composite
physical-field counts use bit 15 to mark records whose enum leaves contain member
ordinals. The remaining bits retain the physical attribute count. The marker
covers nested records and arrays; ordinary text fields retain their original text.
Frozen enum defaults use journal/default tag 42 for a member identity; tag 12
remains readable.
New row writes use journal kind 153, which permits both explicitly tagged member
cells and legacy cells. The preceding row kind remains readable. Even before
the next checkpoint, an older recovery candidate must reject the new row kind
instead of installing opaque bytes it cannot decode.

Bound view SQL uses the non-SQL prefix `pos3ql-bound-query-v1`, followed by
tokenized SQL. Before each typed enum literal, `/*p3e:D:M*/` records dependency
position D and member position M. The writer strips client comments before adding
bindings. Readers resolve these positions through the captured dependencies and
remove the internal grammar before parsing or displaying SQL. Unknown grammar
versions and invalid bindings are errors.

Domain CHECK and stored DEFAULT SQL against an enum or enum-array base use `pos3ql-enum-check-v1`. Its
`/*p3c:M*/` literal bindings refer to base member M; `/*p3a*/` introduces a
text array of base member ordinals, and `/*p3t:N*/` binds N tokens of an explicit
cast type name to the domain's selected base enum. Ordinary text is unbound.
Rendering removes the grammar and resolves current labels and type names.
Defaults also retain their original display text as UTF-8 in
`/*p3o:TEXT*/` immediately after the header. Percent and slash bytes use `%25` and `%2f`
so text cannot terminate the comment. The comment stays unchanged through
schema rewrites. `pg_type.typdefault` and
`information_schema.domains.domain_default` expose this original text. Execution
and column/view default descriptions resolve the captured members.
Domain definitions with bound checks, defaults or named array bases use journal kind 151;
bound views use kind 152;
the previous domain and view kinds remain readable. Tables carrying bound default
source use kind 154; their preceding table kind remains readable. These distinctions
survive prepared-transaction re-encoding. Bound view defaults use column-default
tag 3; the preceding expression tag 2 remains readable.

Legacy enum indexes hashed labels. Loading an older manifest explicitly retires
those generations and schedules replacements; scalar key hashes use stable member
identities rather than labels or sort positions; existing row objects remain
readable. Enum-dependent most-common-value hash summaries from older manifests
or journal kind 77 are retired while their counts and dependency estimates remain
valid. ANALYZE recreates these cached summaries. New summaries use journal kind 150. Label edits and sort renumbering do not rewrite table rows. Upgrade every recovery and replacement candidate before this binary writes
journal kinds 149–154 or manifest v15. Kind 153 applies to ordinary row writes
too, so the upgrade is required before resuming writes, even without enum DDL.

### Composite type name changes

Renaming a composite type or moving it to another schema writes journal kind
148. It contains the prior name, prior schema, new schema and new name, using
the existing length-prefixed name grammar. Replay changes the name while
preserving the current fields. It cannot restore field type names copied before
an earlier domain rename in the same transaction. Attribute edits continue to
use the existing full composite definition records; readers retain those records.

For example, changing a customer identifier domain and then renaming a record
that contains it must recover the new domain reference after losing local caches.
Prepared transactions retain the same name-change records through checkpoints.

Binaries that predate kind 148 cannot recover a journal containing it. Upgrade
all recovery and replacement candidates before executing composite type renames
or schema moves; do not downgrade that prefix afterward. Existing manifest,
composite definition and name-change record grammars remain readable.

Full cast and operator catalog records freeze the transaction's current type
names before journal staging. A domain rename followed by an operator, family
or class owner edit must not restore an old input or result name during replay.
The field inventory is shared with database-scoped type moves. Existing record
grammars are unchanged.

### Prepared relation locks

Prepared-transaction lock payloads identify each entry with a kind byte. Readers
retain kinds 0 (stored-table relation lock), 1 (row lock), and 2 (advisory lock),
and also accept kind 3 for a view relation lock. Writers use kind 3 only for
views; its remaining fields retain the mode and qualified relation name grammar.
Recovery resolves the view's current catalog slot before reinstalling the lock.
Unknown kinds and invalid modes stop recovery explicitly.

For example, a prepared transaction that read a view continues to block a
conflicting rule change after the server loses its local caches. Committing or
rolling back that prepared transaction releases the recovered lock. A binary
that predates kind 3 cannot recover a prepared transaction containing a view
lock; resolve those transactions before downgrading. Old table, row, and
advisory lock payloads remain readable without rewriting their records.

## Compatible online changes

A compatible format change must add a distinct identity and a reader before a
writer can publish that identity. During the compatibility window:

1. the reader accepts every declared old and current identity;
2. new generations use only the current writer identity;
3. one manifest may reference old and current immutable generations together;
4. ordinary slicing and compaction replace old generations without a table-wide
   rewrite; and
5. retries retain the exact source and destination identities.

Manifests v13 and v14 follow the same rule: both are accepted at startup, while
every new manifest is v15. Published v2 and v3 row generations can coexist with v4
generations. New row slices and pair merges write v4 PAX; new deltas write v4
packed rows. A clean older generation may remain reachable indefinitely, so
online replacement does not by itself justify removing its reader.

The reader and writer identities above are executable declarations. A test
compares those declarations with this matrix, so adding or removing an identity
requires updating both in the same change. The v13 compatibility path publishes
an actual legacy manifest, destroys both local cache tiers, recovers its v4 row
generation, publishes v15 after a later commit, destroys the caches again, and
recovers both generations. This qualifies the stated online upgrade instead of
only testing the header parser.

## Incompatible and offline changes

Reader support cannot be removed until an offline migration exists and proves
that no live manifest reference uses the retired identity. Such a migration
must operate with the database stopped, write new immutable objects, verify
them through empty caches, and publish the replacement manifest with the same
compare-and-swap ownership rule as a checkpoint. The old reachable objects
remain intact until the replacement root is durable and verified.

There is currently no incompatible durable-format migration and no format is
eligible for reader removal. A future incompatible row representation must
ship its migration procedure and recovery tests before its writer is enabled.
Forward formats, malformed references, and manifests outside the matrix fail
startup explicitly.

## Change checklist

A durable-format change is complete only when it includes:

- a new stable identity and one parse boundary for it;
- mixed-generation reads and object-cold recovery;
- checkpoint retry and compare-and-swap publication coverage;
- exact fixed-memory accounting for readers, writers, and migration scratch;
- garbage retention for both the published root and in-progress output;
- upgrade and downgrade behavior in this matrix; and
- a manifest-capacity assessment for any added reference bytes.

## Row identity exhaustion

For example, inserting an order needs a new internal row identity; updating an
existing order in the same table retains its identity. The allocator reserves
`u64::MAX` as a terminal next value and returns `54000` (`row identities
exhausted`) instead of wrapping. Failed inserts can leave gaps, and those gaps
are never reused during the process lifetime. This is not a configurable limit.

The manifest's existing unsigned `next_rowid` field preserves this terminal
value across recovery. Journal and saved-file observations advance the counter
with saturating arithmetic, including legacy maximum identities. Row key and
manifest encodings are unchanged; the allocator adds no startup reservation.
