# Durable format compatibility

Use this guide to identify the formats the current binary reads and writes.
Compatibility with earlier pos3ql versions is not required. Existing older
readers are listed as implemented behavior, not a promise to retain them.
No automatic upgrade, downgrade, or offline migration is promised.

**User example:** after an incompatible format change, a database saved by an
older pos3ql binary may fail startup. Use a fresh object-store prefix or an
explicitly verified logical export and restore between the chosen binaries.
Never point a new database at an old prefix and assume its objects are disposable.
Unknown formats stop startup explicitly.

Object storage is the durable tier. Immutable objects may outlive the binary
that wrote them, so every root and row SST carries a format identity that is
checked before its bytes are interpreted. Unknown identities stop startup;
they never select a guessed decoder.

## Current compatibility matrix

| Object | Identity | Read | Write | Migration |
|---|---|---:|---:|---|
| Checkpoint manifest | `pos3ql-manifest-v18` | yes | yes | Requires durable type slots/lifetimes, a catalog identity counter and a completed type-loading boundary; retains typed blocks and enum member identities. |
| Journal record | Version `2` at header byte 17; bytes 18–23 are zero | yes | yes | Older framing is rejected even when its checksum is valid. |
| Immutable block | Header bytes 5–7: `01 00 00` | yes | yes | SHA-256 of `pos3ql:block:v1\0`, the one-byte block type, then payload; LSN is provenance, excluded from identity. |
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

A column container and a row index can contain identical bytes and still be
different blocks. Their type is part of the object name, so writing one cannot
replace the other. Headers without the current identity version are rejected.
Manifest v17 and older manifests are rejected before their references are used.

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
reads and validates both mutable root envelopes, including required manifest
metadata, in its startup buffers. Invalid formats fail before changing the
writer fence, so they cannot displace the existing writer. It then
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
Both roots are read again during fencing, so commits arriving after preflight
are preserved. Once a valid promotion starts, an interrupted transition is taken
over and completed by the next startup.

Commit-head v1 readers accept both legacy 64-bit writer labels and current
128-bit process tokens. A prefix without `writer-fence` is promoted in place.
Promotion replaces the manifest's validated 32-digit hexadecimal writer identity
with one of the same width and updates the commit head. Database references
are preserved while the root bytes and ETags change. A missing manifest writer
is an error; promotion does not insert one.

The manifest writer line is charged to `checkpoint_manifest_bytes`. The mutable
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
and view kinds. Stored-query dependencies use marker `0xfd`, a length-prefixed
identity and the complete column set. Type identities use `d:<slot>:<creation>`,
`e:<slot>:<creation>` or `c:<slot>:<creation>`; other identities retain their
decimal object identifiers or zero for name-based objects. Earlier dependency
grammars are rejected. Table statistics use the
v4 marker 253 for 16-bit counts and ordinals while retaining v3 marker 254.
The current manifest text represents sparse column sets and streams wide
composite fields into the configured manifest buffer.

Table constraints are fixed at 64 entries per modeled kind and
domain checks at 64 entries. Their journal counts are unsigned bytes, while
their positions use a 64-entry stride in synthesized `pg_constraint` and
referential-trigger OIDs. The 65th entry is rejected with SQLSTATE `54000`
before catalog mutation. Raising this boundary would renumber durable catalog
identities, so it requires an explicit format identity and a review of catalog
OID ranges rather than only changing the constant.

### Enum member identities

An enum member's position in its catalog member list is its stable identity.
Adding a member appends to that list, even when its displayed sort position is
BEFORE another member. Renaming or renumbering never changes an existing member's
identity. Row values keep this identity separately from their cached label and
sort position. Catalog-aware reads select the current label and sort position.

Manifest v15 adds an optional legacy label after each enum member's sort bits.
The retained enum payload readers for kinds 23/147 initialize this binding from their
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
Their labels obey the [PostgreSQL 18 limit of 63 bytes](https://www.postgresql.org/docs/18/datatype-enum.html#DATATYPE-ENUM-IMPLEMENTATION-DETAILS).
Object-reference defaults such as `'orders'::regclass` retain the full type and
target OIDs in the encoded record. Their memory cache uses a closed type tag;
unknown type OIDs fail decoding. The largest encoded default is 130 bytes,
including its tag and a 120-byte cached object name. The reference-type set
follows [PostgreSQL 18's object identifier types](https://www.postgresql.org/docs/18/datatype-oid.html).
New row writes use journal kind 153, which permits both explicitly tagged member
cells and legacy cells. The preceding row kind remains readable. Even before
the next checkpoint, an older recovery candidate must reject the new row kind
instead of installing opaque bytes it cannot decode.

Bound view SQL uses the non-SQL prefix `pos3ql-bound-query-v1`, followed by
tokenized SQL. Before each typed enum literal, `/*p3e:D:M*/` records dependency
position D and member position M. The writer strips client comments before adding
bindings. The parser analyzes ordinary value strings as parameters while
preserving grammar literals, including JSON_TABLE paths and typed date constants.
Explicit cast types are retained before CTE expansion can discard their original
expressions; captured type dependencies remain authoritative after name reuse.
Readers resolve the bound positions through the captured dependencies and remove
the internal grammar before parsing or displaying SQL. Unknown grammar
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

Legacy enum indexes hashed labels. Legacy index records explicitly retire
those generations and schedules replacements; scalar key hashes use stable member
identities rather than labels or sort positions; existing row objects remain
readable. Enum-dependent most-common-value hash summaries from older manifests
or journal kind 77 are retired while their counts and dependency estimates remain
valid. ANALYZE recreates these cached summaries. New summaries use journal kind 150. Label edits and sort renumbering do not rewrite table rows. Upgrade every recovery and replacement candidate before this binary writes
current journal records or manifest v18. Kind 153 applies to ordinary row writes
too, so the upgrade is required before resuming writes, even without enum DDL.

Journal routine type references identify their saved source lifetime. Replay
refreshes the SQL spelling from that source; a missing lifetime cannot select a
replacement with the same name.

### Typed tables

A typed table saves its composite source as `c:<slot>:<creation>`, without a
duplicate SQL name. Recovery checks the source lifetime and column shape before
installing the table. Dropping a type and creating an identical replacement in
the same slot cannot make a saved definition select that replacement. Template
copies deliberately bind this link to the new database’s cloned type.

Saved queries use the same source keys for domain, enum and composite
dependencies. A view recorded before a rename can commit afterward and recover
the renamed source. Its original spelling cannot select a replacement. Routine
publication validates these dependencies under the same owners as its signature
and dependency image.

### Type lifetimes and saved references

**User example:** one connection creates a cast using `order_code` inside a
transaction. Another connection renames that type to `current_code` and commits
first. The first connection can then commit. After restart, the cast still
refers to that same type, now named `current_code`. Its saved name cannot select
a replacement that reused `order_code`.

Manifest v18 and current journal records preserve the physical slot and
`created_at` identity of each domain, enum and named composite. The required
`catalog_sequence` manifest field remembers the highest creation number used,
including objects deleted before checkpoint. Restore never assigns
a fresh lifetime to a saved type. Slots are checked against configured capacity;
zero creation identities, mismatched lifetimes and unavailable slots are errors.

The manifest writes `dom5`, `enm2` and `cmp2` records before a required
`types_end` record. At that boundary, recovery resolves domain chains and
composite fields in each database before loading dependent definitions. Missing
or duplicate boundaries and later type records are rejected.

Every stored user-type name has an explicit prefix. `r:<kind>:<slot>:<creation>:<name>`
identifies a bound type; kinds `d`, `e` and `c` mean domain, enum and composite.
`n:<name>` is a declaration requiring binding; `p:<name>` is a validated
PostgreSQL polymorphic type. The schema is stored separately and database scope
comes from the enclosing record. The SQL name after the prefix retains its
original colons and Unicode bytes. Manifests hex-encode these fields. Tables,
composites, routine signatures/results, aggregate types, casts and operator
contracts share this encoding. Table definition payloads use version 6.

Journal framing remains 24 bytes: CRC32C, payload length and LSN occupy bytes
0–15; byte 16 selects the operation; byte 17 is version 2; bytes 18–23 are zero.
A checksum-valid unsupported version stops local replay without truncating the
journal. Uploaded records validate the same version before decoding. Update all
recovery candidates together and use a fresh prefix or verified logical transfer
from an incompatible binary; no automatic conversion is provided.

## Composite type name changes

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
or schema moves; do not downgrade that prefix afterward. The current manifest and journal identities above apply to these records too.

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

## Reader changes

A format change declares its accepted reader identities and current writer
identity together. Backward compatibility with earlier pos3ql versions is not
required: obsolete readers may be removed without building an offline migration.
Do not retain a decoder or introduce a conversion fallback solely to support
an older pos3ql version.

The only accepted checkpoint manifest is v18. Startup, backup retention, and
writer promotion share one allocation-free metadata parser. It requires one
log position, next row identity, latest transaction identity, large-object
allocator record, nonzero 32-digit hexadecimal writer identity and final
terminator. Missing, repeated, malformed or trailing metadata fails explicitly,
before catalog restoration or root publication. Promotion never inserts a
missing writer identity.

There is no old-manifest writer, mode selector, or conversion that retires enum
indexes during recovery. Current enum identities and index references are
restored directly. For example, a saved enum value still reads as its renamed
label after loss of local caches; changing or removing its saved manifest
identity stops startup without overwriting that root.

The row readers still accept v2, v3, and v4 as listed above; this manifest change
does not remove their codecs. A test compares the executable reader and writer
declarations with the matrix, so changing an identity requires changing both
declarations and tests.

Recovery within the current version must remain correct: reachable objects,
prepared transactions, and backup roots written by that version must be readable.
A checkpoint or retry may not discard objects still needed by current readers
or backups. Unknown identities, malformed references, and manifests outside the
matrix fail explicitly before their bytes are interpreted.

## Change checklist

A durable-format change is complete only when it includes:

- a new stable identity and one parse boundary for it;
- reads of every declared format and object-cold recovery;
- checkpoint retry and compare-and-swap publication coverage;
- exact fixed-memory accounting for readers, writers, and any conversion scratch;
- garbage retention for both the published root and in-progress output;
- accepted and rejected identities in this matrix; and
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
