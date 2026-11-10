# Finding rows without reading every storage object

Indexes narrow the data a query inspects. pos3ql stores them in immutable
objects rather than PostgreSQL index pages. This guide covers the
specialized GiST, SP-GiST, and GIN indexes; btree and hash behavior is in the
[compatibility table](postgresql-18-compatibility.md#physical-access-paths),
and their place beside row generations, PAX column groups and caches is in
[architecture](architecture.md#reads-and-storage-layout).

**User example:** find orders containing a tag:

```sql
CREATE TABLE tagged_orders (id integer PRIMARY KEY, tags text[]);
INSERT INTO tagged_orders VALUES (1, ARRAY['urgent']), (2, ARRAY['gift']);
CREATE INDEX tagged_orders_tags ON tagged_orders USING gin (tags);
SELECT id FROM tagged_orders WHERE tags @> ARRAY['urgent'];
```

The result is order `1`. On a large enough table, GIN uses the `urgent` token
to narrow candidate rows, then checks the actual array and transaction
visibility, so summaries and token collisions cannot change the answer. A
small table may use a sequential scan.

"Pruning" means safely skipping data that cannot match: spatial indexes skip
regions by bounding box, range indexes skip disjoint intervals. Pruning
reduces work; it does not guarantee a request count or query rate.

## How objects are organized

| Classes | Summary |
|---|---|
| GiST point, box, polygon, circle; SP-GiST quad-point, k-d point, box, polygon | Bounding boxes |
| GIN array, `tsvector`, `jsonb_ops`, `jsonb_path_ops` | Hashed-token posting trees |
| GiST `tsvector` | 256-bit token signatures |
| GiST range, multirange, network; SP-GiST range, network | Ordered interval envelopes |

These are pos3ql object formats, not PostgreSQL page storage or an
operator-class callback interface.

Checkpoints sort geometric keys by bounding-box center, GIN postings by token
key, and other keys by their deterministic encoding. Target block sizes:
geometric 16 KiB, signature 1 KiB (keeps signatures selective), interval and
posting 8 KiB; one oversized entry gets its own block. Parent nodes have at
most 32 children and store descendant entry counts and merged summaries. The
value-index manifest handle names the root and published LSN. Legacy flat or
linked rosters stay readable, without pruning, until a checkpoint replaces
them.

## Durable format

`ValueIndexNavigationV1` is block kind 17. `ValueIndexPostingV1` is block kind
18 with the same node layout; the separate kind tells readers which format
they hold. All nodes in one tree share a block kind. Counters, coordinates,
and signature words are little-endian; interval and posting keys are
order-preserving byte strings compared lexicographically. Content addressing
and integrity use the provider-neutral block store.

| Field | Bytes | Meaning |
| --- | ---: | --- |
| Header | 8 | Version 1; height; indexed key position; covering flag; 16-bit child count; two zero reserved bytes |
| Child identity | 32 | Nonzero immutable block identifier |
| Child entry count | 8 | Positive descendant count |
| Child summary | 33 | Tag plus four binary64 coordinates, a 256-bit token signature, or two 15-byte interval keys and flags |

Height zero points to value-index data blocks; greater heights point to nodes
exactly one level lower. Only an empty generation's root may have zero
children.

| Summary tag | Meaning | Payload |
|---:|---|---|
| 0 | All keys NULL | none |
| 1 | Unbounded | none |
| 2 | Finite ordered rectangle | four binary64 coordinates |
| 3 | Token signature | 256 bits |
| 4 | Interval envelope | flags, inclusive lower and upper keys, one reserved zero byte |

Interval flags distinguish nonempty values from empty ranges, so empty values
never become false negatives when summaries merge. Non-finite keys use
unbounded bounds and always get exact rechecks; non-finite or reversed
coordinates under tag 2 are corruption. Readers reject inconsistent height,
key position, covering flag, entry counts, aggregate summaries, lengths, and
fan-out. Mixed finite, signature, and interval children merge to unbounded,
so a malformed writer can only reduce pruning, never cause a false negative.
Posting nodes also require a non-covering header and bounded interval
summaries with zero padding; mixed block kinds or summary forms are
corruption.

A posting leaf uses the ordinary value-index data-block encoding. Its key is a
one-byte token namespace plus an eight-byte big-endian stable hash; its
identity fields are the row identity and commit LSN. Postings are lossy
candidates, not a second source of truth: the original value and any hash
collision are resolved by the normal SQL and MVCC (row-version visibility)
rechecks. GIN `INCLUDE` columns are rejected, so postings carry no covering
payload.

## Memory and correctness

The builder reserves fourteen 2,344-byte nodes at startup and reuses them; a
fixed-depth cursor holds at most 435 child references. Neither allocates at
runtime. Fourteen levels at fan-out 32 cover the full unsigned 64-bit entry
space.

Predicates skip children with disjoint bounds before fetching them. Bounds
allow for PostgreSQL's geometric tolerance and floating-point rounding, and
selected keys still get exact SQL operator and MVCC checks.

- **GiST full-text signatures** set three deterministic bits per lexeme.
- **GIN postings** put array elements, full-text lexemes, JSON keys, JSON
  strings/literals, and top-level JSON existence keys in separate namespaces.
  Containment and conjunctions may use one required token; overlap, JSON
  `?|`, and full-text OR union every positive branch and deduplicate rows.
  Prefix-only and negation-only full-text terms, numeric-only JSON, JSONPath,
  empty-token predicates, and array contained-by (`<@`) do not use postings.
  Repeated tokens in one value collapse to one posting. Legacy GIN signature
  generations are recognized by block kind and use ordinary execution until a
  checkpoint replaces them.
- **Interval summaries** cover every built-in range subtype, multiranges, and
  the network-to-broadcast span of IPv4 and IPv6 values. Integer, date, and
  timestamp keys keep exact order; numeric keys and the last IPv6 byte are
  conservative prefixes, so collisions only keep extra children. Equality,
  containment, overlap, and scalar range containment skip disjoint children;
  positional range predicates keep all children.

Uncommitted keys visible to the transaction are evaluated alongside published
keys. Garbage collection walks every parent and leaf regardless of pruning.

Tests cover three-level reads, recycled and empty builders, malformed
encodings, every summarized class with empty caches, object GET and byte
budgets, rollback and committed overlays, repeated checkpoint and garbage
collection, wide expression keys, escaped JSON strings, boolean full-text
queries, and allocation-forbidden execution. PostgreSQL 18.6 is the SQL
oracle in `tests/external/differential/173_geometric_navigation.sql` through
`176_gin_postings.sql`. Warm-memory and cold-object performance artifacts
cover every signature, posting, and interval operator-class path.

## Nearest results and when early stopping is safe

**User example:** `ORDER BY location <-> '(0,0)'::point LIMIT 10` asks for
the ten nearest points. An eligible geometric index stops exploring regions
too far away to change that top ten. If the query also filters rows or skips
locked rows, stopping early could return fewer than ten valid matches, so
those queries use complete exact ordering.

Finite, unfiltered `<-> point` limits on built-in GiST and SP-GiST geometric
classes use a bounded depth-first branch-and-bound cursor:

- Children are visited in order of their point-to-box lower bound, widened
  for geometric tolerance so it can only keep extra nodes.
- A fixed max-heap in the statement arena keeps `LIMIT + OFFSET` row
  identities and tightens the pruning radius.
- Committed resident keys are ranked first; stale durable versions fail the
  normal MVCC check; NULL keys sort last.
- Only winning key and `INCLUDE` payloads are re-read, from their remembered
  leaves, which are normally still in cache.
- The frontier is bounded by tree depth and fan-out, and traversal does not
  allocate.

Exact `<->` evaluation decides the final order. These cases use the complete
exact plan instead: legacy rosters, non-finite origins, no or unbounded limit,
residual predicates, row-level security, row locking, `WITH TIES`, and top-k
scratch requests that exceed their reservation.
