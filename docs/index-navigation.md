# Immutable specialized-index navigation

GiST point, box, polygon, and circle classes and SP-GiST quad-point, k-d point,
box, and polygon classes use bounding-box summaries. GIN array, `tsvector`,
`jsonb_ops`, and `jsonb_path_ops` classes use exact lossy-token posting trees;
GiST `tsvector` uses token signatures. GiST range, multirange, and network
classes and SP-GiST range and network classes use ordered interval envelopes.
These are object-native formats, not PostgreSQL page storage or a native
operator-class callback interface.

Checkpoint construction sorts geometric keys by bounding-box center, GIN
postings by their token key, and other keys by their deterministic encoding.
Geometric data blocks target 16 KiB; signature blocks target 1 KiB to keep
256-bit summaries selective; interval and posting blocks target 8 KiB. One
larger valid entry occupies its own block. Immutable
parent nodes have at most 32 children and retain descendant entry counts and
merged summaries. Unchanged value-index manifest handles name the root and
published LSN. Legacy flat/linked rosters remain readable until checkpoint
replacement.

## Durable format

`ValueIndexNavigationV1` has block-kind code 17. `ValueIndexPostingV1` has
block-kind code 18 and uses the same node layout while giving readers an
unambiguous durable-format discriminator. Every node in one tree must have the
same block kind. Multi-byte counters, spatial coordinates, and signature words
are little-endian; interval and posting keys are order-preserving byte strings
compared lexicographically. Content-addressing and block integrity use the
existing provider-neutral block-store boundary.

| Field | Bytes | Meaning |
| --- | ---: | --- |
| Header | 8 | Version 1; height; indexed key position; covering flag; 16-bit child count; two zero reserved bytes |
| Child identity | 32 | Nonzero immutable block identifier |
| Child entry count | 8 | Positive descendant count |
| Child summary | 33 | Tag plus four binary64 coordinates, a 256-bit token signature, or two 15-byte interval keys and flags |

Height zero points to value-index data blocks. Greater heights point to nodes
exactly one level lower. Only an empty generation's root admits zero children.
Summary tag zero means all keys are NULL, one means unbounded, two means a
finite ordered rectangle, three means a token signature, and four means an
interval envelope. The first two tags require zero payload bytes. An interval
stores flags followed by inclusive lower and upper order keys; the final byte
is reserved and must be zero. Its flags distinguish an envelope containing
nonempty values from one containing empty ranges, so empty values never become
false negatives when summaries merge.
Non-finite keys use unbounded bounds and always receive exact rechecks;
non-finite or reversed coordinates under the finite tag are corruption.
Readers reject inconsistent height, position, covering flags, entry counts,
aggregate summaries, lengths, and fan-out rather than interpreting another
format. Mixed finite, signature, and interval children merge to unbounded, so
a malformed writer can reduce pruning but cannot create a false negative.
Posting nodes additionally require a non-covering header and bounded interval
summaries with canonical zero padding; mixed block kinds or summary forms are
corruption.

A posting leaf uses the ordinary immutable value-index data-block encoding.
Its key is a one-byte token namespace followed by an eight-byte big-endian
stable hash; its identity fields are the row identity and commit LSN. Parent
summaries carry the inclusive token-key interval in the existing 15-byte
envelope. The original indexed value and any hash collision are resolved by
the ordinary SQL and MVCC rechecks, so postings are a lossy candidate structure
rather than a second source of truth. GIN included columns are rejected at the
SQL boundary and posting entries therefore have no covering payload.

## Memory and correctness

The builder reserves fourteen 2,344-byte nodes at startup and reuses them
between generations. A fixed-depth cursor retains at most 435 child references;
neither operation allocates or grows runtime memory. Fourteen levels cover the
complete unsigned 64-bit entry space at fan-out 32.

Predicates reject disjoint child bounds before fetching those objects. Bounds
are conservative around PostgreSQL's geometric tolerance and floating-point
rounding; selected keys still receive exact SQL operator and MVCC checks.
GiST full-text signatures set three deterministic bits per lexeme. GIN posting
extraction gives array elements, full-text lexemes, JSON keys, JSON
strings/literals, and top-level JSON existence keys separate namespaces.
Containment and conjunctions may use one required token; overlap, JSON `?|`,
and full-text disjunctions union every positive branch and deduplicate row
identities. Prefix-only and negation-only full-text terms, numeric-only JSON,
JSONPath evaluation, empty-token predicates, and array contained-by decline
posting navigation when no exact token can conservatively bound candidates.
Repeated tokens in one indexed value collapse to one durable row/version
posting during the checkpoint sort.

Legacy GIN signature generations are recognized by block kind and use ordinary
execution until checkpoint replacement; they are never interpreted as posting
keys.
Interval summaries cover the outer bounds of every built-in range subtype,
multiranges, and the network-to-broadcast span of IPv4 and IPv6 values. Integer,
date, and timestamp keys preserve exact order; numeric keys and the final IPv6
byte are conservative prefixes, so collisions only retain extra children.
Equality, containment, overlap, and scalar range containment reject disjoint
children. Positional range predicates retain all children until a richer
two-sided proof is available. Empty and unbounded values have explicit
conservative behavior, and every retained key receives the normal exact SQL
operator check.
Transaction-visible resident overlays are evaluated alongside published keys.
Covering payloads remain intact. Garbage collection walks every parent and
leaf, independent of predicate pruning.

Qualification includes three-level bounded reads, recycled and empty builders,
malformed encodings, every summarized class with empty caches, measured object
GET/byte budgets, rollback and committed overlays, repeated checkpoint and
garbage-collection cycles, wide expression keys, escaped JSON strings, boolean
full-text queries, and allocation-forbidden execution. PostgreSQL 18.6 supplies
the SQL oracle in `tests/external/differential/173_geometric_navigation.sql`
through `176_gin_postings.sql`. Warm-memory and cold-object performance
artifacts cover every signature, posting, and interval operator-class path
independently.

Ranked nearest-neighbor node traversal remains in [PLAN.md](../PLAN.md).
Unfiltered nearest-neighbor queries currently order complete compact candidate
sets; geometric predicates prune candidates without changing that limit boundary.
