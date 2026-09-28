# Durable format compatibility

Object storage is the durable tier. Immutable objects may outlive the binary
that wrote them, so every root and row SST carries a format identity that is
checked before its bytes are interpreted. Unknown identities stop startup;
they never select a guessed decoder.

## Current compatibility matrix

| Object | Identity | Read | Write | Migration |
|---|---|---:|---:|---|
| Checkpoint manifest | `pos3ql-manifest-v13` | yes | no | The next successful checkpoint publishes v14. |
| Checkpoint manifest | `pos3ql-manifest-v14` | yes | yes | Current format. |
| Backup completion | `pos3ql-backup-v1` | yes | no | Named-point and LSN restore remain readable; no backup creation timestamp is available. |
| Backup completion | `pos3ql-backup-v2` | yes | yes | Checksums both roots and anchors timestamp recovery after backup creation. |
| Point-in-time restore marker | `pos3ql-point-in-time-restore-v1` | yes | yes | Binds the backup, manifest checksum, resolved LSN, and commit head during root replacement. |
| Backup export marker | `pos3ql-export-v1` | yes | yes | Pending and completion records make an independent-prefix copy restartable. |
| Published row SST | `v2` | yes | no | Compatible generations remain readable and are replaced when sliced or merged. |
| Published row SST | `v3` | yes | no | Compatible packed PAX generations remain readable. |
| Published row SST | `v4` | yes | yes | Current packed format: PAX full slices and row-packed deltas. |

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

Current commit boundary records append PostgreSQL-epoch commit microseconds to
the transaction identity fields. Readers retain the empty, identity-only, and
identity-plus-assignment legacy forms. LSN recovery can use legacy records;
timestamp recovery rejects a legacy boundary whose time cannot be known.
Recovery may write an immutable prefix of a multi-transaction commit batch.
Its identity is the CRC32C of the exact prefix and its descriptor preserves the
original predecessor, so the published head ends only at a validated whole
transaction.

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

## Compatible online changes

A compatible format change must add a distinct identity and a reader before a
writer can publish that identity. During the compatibility window:

1. the reader accepts every declared old and current identity;
2. new generations use only the current writer identity;
3. one manifest may reference old and current immutable generations together;
4. ordinary slicing and compaction replace old generations without a table-wide
   rewrite; and
5. retries retain the exact source and destination identities.

Manifest v13 follows the same rule: it is accepted at startup, while every new
manifest is v14. Published v2 and v3 row generations can coexist with v4
generations. New row slices and pair merges write v4 PAX; new deltas write v4
packed rows. A clean older generation may remain reachable indefinitely, so
online replacement does not by itself justify removing its reader.

The reader and writer identities above are executable declarations. A test
compares those declarations with this matrix, so adding or removing an identity
requires updating both in the same change. The v13 compatibility path publishes
an actual legacy manifest, destroys both local cache tiers, recovers its v4 row
generation, publishes v14 after a later commit, destroys the caches again, and
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
