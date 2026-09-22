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

Block headers also carry a typed block identity. A row SST format defines the
allowed index-entry shape and block types together. A recognized block type in
the wrong row SST format is corruption rather than an alternate decoding path.

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
