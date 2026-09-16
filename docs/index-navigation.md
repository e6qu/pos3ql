# Geometric index navigation

GiST point, box, polygon, and circle classes and SP-GiST quad-point, k-d point,
box, and polygon classes share one object-native bounding-box tree. This is
not PostgreSQL page storage or a native operator-class callback interface.

Checkpoint construction sorts compact keys by bounding-box center, then by
encoded key for a deterministic tie-breaker. Data blocks target 16 KiB; one
otherwise valid larger entry occupies its own block. Immutable parent nodes
have at most 32 children and retain descendant entry counts and bounds.
Unchanged value-index manifest handles name the root and published LSN.
Legacy flat/linked rosters remain readable until checkpoint replacement.

## Durable format

`ValueIndexNavigationV1` has block-kind code 17. Every integer and floating-point
coordinate in its payload is little-endian. Content-addressing and block
integrity use the existing provider-neutral block-store boundary.

| Field | Bytes | Meaning |
| --- | ---: | --- |
| Header | 8 | Version 1; height; indexed key position; covering flag; 16-bit child count; two zero reserved bytes |
| Child identity | 32 | Nonzero immutable block identifier |
| Child entry count | 8 | Positive descendant count |
| Child bounds | 33 | Tag plus four binary64 coordinates: minimum X/Y and maximum X/Y |

Height zero points to value-index data blocks. Greater heights point to nodes
exactly one level lower. Only an empty generation's root admits zero children.
Bounds tag zero means all keys are NULL, one means unbounded, and two means a
finite ordered rectangle. The first two tags require zero coordinate bytes.
Non-finite keys use unbounded bounds and always receive exact rechecks;
non-finite or reversed coordinates under the finite tag are corruption.
Readers reject inconsistent height, position, covering flags, entry counts,
aggregate bounds, lengths, and fan-out rather than interpreting another format.

## Memory and correctness

The builder reserves fourteen 2,344-byte nodes at startup and reuses them
between generations. A fixed-depth cursor retains at most 435 child references;
neither operation allocates or grows runtime memory. Fourteen levels cover the
complete unsigned 64-bit entry space at fan-out 32.

Predicates reject disjoint child bounds before fetching those objects. Bounds
are conservative around PostgreSQL's geometric tolerance and floating-point
rounding; selected keys still receive exact SQL operator and MVCC checks.
Transaction-visible resident overlays are evaluated alongside published keys.
Covering payloads remain intact. Garbage collection walks every parent and
leaf, independent of predicate pruning.

Qualification includes three-level bounded reads, recycled and empty builders,
malformed encodings, all eight classes with empty caches, rollback and committed
overlays, repeated checkpoint/garbage-collection cycles, wide expression keys,
and allocation-forbidden execution. PostgreSQL 18.6 supplies the SQL oracle in
`tests/external/differential/173_geometric_navigation.sql`; its native GiST NaN
mislookup and quad-point construction error are isolated from the finite-index
comparisons. Unindexed non-finite SQL remains differential-tested, and Rust
cold-tree tests mix finite, infinite, NaN, and NULL keys in our durable format.

Network, range/multirange, and full-text navigation, GIN posting structures,
and ranked nearest-neighbor node traversal remain in [PLAN.md](../PLAN.md).
Unfiltered nearest-neighbor queries currently order complete compact candidate
sets; geometric predicates prune candidates without changing that limit boundary.
