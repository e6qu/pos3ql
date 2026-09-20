# Implemented baseline

This record preserves the completed implementation evidence from the roadmap
through the JSON value-width work merged on 2026-09-20. The active production
roadmap is [PLAN.md](../PLAN.md).

- Object-native commit publication, checkpoints, crash recovery, and recovery
  with empty RAM and disk caches.
- A bounded S3-compatible client covering conditional writes, full and ranged
  reads, paginated listing, deletion, opaque entity tags, TLS, signing, retry
  classification, and structured errors through one endpoint-independent path.
- Bounded RAM and local-disk cache tiers over immutable object data.
- PostgreSQL v3 simple and extended query flows, common drivers, COPY,
  cancellation, TLS, authentication, notifications, and logical-replication
  mode.
- Broad PostgreSQL 18 SQL, type, procedural, catalog, dump/restore, and tooling
  compatibility. The command inventory records both executable behavior and
  deliberate typed rejection; it is not a claim that every PostgreSQL grammar
  production or server subsystem is implemented.
- Object-native btree, hash, BRIN, GiST, GIN, and SP-GiST access paths, including
  durable generations, transaction overlays, WAL, checkpoints, object-cold
  recovery, and the documented built-in operator families.
- PostgreSQL logical publication/subscription and pgoutput interoperability.
  These create logical data copies, not physical or shared-storage replicas.
- Fixed-startup memory accounting, deterministic fault simulation, differential
  tests, vendored PostgreSQL regression slices, SQLLogicTest corpora, driver
  probes, and performance-smoke checks.
- Checkpoint, merge, pending-install, and temporary-spill bookkeeping sized from
  the configured physical-table capacity rather than a 1,024-slot ceiling.
  Above-boundary tables, dropped identities, fresh replacement slots, repeated
  publication, an ambiguous compare-and-swap response, and empty-cache recovery
  share one regression. The complete manifest has an explicit startup-reserved
  `checkpoint_manifest_bytes` bound and fails before publication when full.
  Commit-chain replay, the live content-addressed block keep-set, SST-pair merge
  scheduling, and garbage deletion batches have independent startup capacities.
  Garbage beyond one batch is drained over paced beats instead of becoming a
  scale error. Manifest publication is handed to local WAL, heap, and overlay
  cleanup in the same beat before a newer statement can run; remote deletion
  remains paced, while explicit checkpoints wait for it to complete.
  Commit-batch pruning drains histories larger than one batch while retaining
  the replay boundary. The sorted live-block set gives each listed-object
  membership test logarithmic cost. Exact memory deltas, named exhaustion,
  multi-batch cleanup, publication interleaving, multi-generation overlay
  compaction, commit-chain cold recovery, and recovery after cleanup share
  regressions.
  Startup-sized table, constraint/default/statistics, publication, replication,
  subscription, dependency, trigger, sequence, and information-schema catalogs
  construct rows from their transaction-visible cardinality instead of hidden
  static arrays. An empty stored-query dependency graph no longer imposes an
  unrelated 128-table drop limit.
  Row MVCC command history and committed snapshot history use startup-sized
  global pools governed by `max_row_versions_per_row`, with constant-time slot
  reuse and explicit per-row and global exhaustion. Per-table immutable SST
  rosters, checkpoint publication and retry scratch, paced merge state,
  temporary spill, and cold-read cursors share
  `max_spill_generations_per_table`; no path retains or truncates to a compiled
  eight-generation list. Regressions cross the old boundary through command
  snapshots, savepoint rollback and reuse, active repeatable-read snapshots,
  ten durable deltas, checkpoint publication, empty-cache recovery, and exact
  startup accounting.
- Extended-statistics objects draw from the independent startup-sized
  `max_extended_statistics` catalog rather than an eight-object-per-table
  array. CREATE, LIKE INCLUDING STATISTICS, schema evolution, DROP cascades,
  ownership graphs, ANALYZE, WAL, checkpoints, and catalogs traverse the full
  configured pool without fixed scratch. BRIN unsummarized ranges live in one
  startup-reserved pool governed by
  `max_brin_unsummarized_ranges_per_index`; the durable one-byte count admits
  up to 255 ranges and every WAL/checkpoint/rebuild path uses the configured
  slice. One regression fills ten statistics slots and seventy trigger and
  BRIN slots, verifies named exhaustion, loses an ambiguous checkpoint reply,
  retries publication, removes local state, and exercises the recovered
  catalogs, triggers, BRIN maintenance, and rows from object storage.
- Database and schema catalogs are independently startup-sized through
  `max_databases` and `max_schemas`, with their complete memory cost charged
  before serving. Database connection counters, cumulative statistics,
  cloning, catalogs, WAL, checkpoints, publication schema membership, and
  object-cold recovery cover the declared capacities. A single `DROP SCHEMA`
  accepts the parser's complete bounded target list, and bulk tablespace,
  REINDEX, and CLUSTER scratch is sized from actual configured table
  cardinality rather than an unrelated schema/column product.
- Sequence catalogs are startup-sized through `max_sequences` across durable
  definitions, per-session `currval`/`lastval` and cache state, dependency
  planning, `DROP OWNED`, identity cleanup, catalogs, WAL, checkpoints, and
  object-cold recovery. Recycled connections retain their startup allocation
  while clearing every sequence slot. The currently disjoint relation-OID
  range admits up to 5,000 sequence slots and rejects larger configurations.
  `max_ddl_per_transaction` now sizes live and prepared transaction undo,
  subscription apply, commit scratch, and logical decoding together, so atomic
  catalog operations above the former 64-change boundary remain usable and
  exactly charged before serving.
- Domain, enum, and named-composite catalogs are independently startup-sized
  through `max_domains`, `max_enums`, and `max_composites`. DDL dependency
  selection, domain-chain recovery, planner record shapes, PostgreSQL and
  information-schema catalogs, routines, views, WAL, checkpoints, and
  empty-cache recovery follow those declared bounds. Schema-less spill and
  constant-default records carry user-array kind separately from its 16-bit
  runtime slot, so identities above the former 32-entry ceiling survive sort,
  replay, and recovery. Accepted domain, enum, composite, table, and view
  capacities are checked against disjoint `pg_type` OID bands before serving.
- Wide schema definitions no longer encounter narrower storage-only limits:
  table constraint kinds and domain checks use the parser's complete 64-item
  bounded list, named composites use the 64-column row boundary, partition
  keys and index tuples use PostgreSQL 18's exact 32-attribute limits, and
  LIST bounds use the complete 64-value parser list. Direct inheritance
  parents, defaults across every view column, subscription publication names,
  event-trigger tags, foreign OPTIONS clauses, and operator-family operators
  and support functions also use the complete parser list. Constraint
  enforcement, record shapes, PostgreSQL catalogs, WAL, checkpoint manifests,
  loud over-boundary errors, and empty-cache object recovery share those
  limits.
  New index WAL uses 32-bit masks while legacy 8-bit WAL remains readable;
  textual checkpoint masks remain backward-readable.
  Wide table/domain/operator-family/operator-class/foreign-table WAL staging
  borrows definitions, decoder branches isolate fixed scratch, and manifest
  replay transfers pending table ownership at one choke point rather than
  reserving a copy in every branch. Schema-only catalog
  resolution reads one shared, storage-independent definition rather than
  constructing and discarding rows. Catalog-backed view descriptions cannot
  recursively materialize their own catalogs. Procedural count passes recycle
  temporary query state.
  Inherited ALTER uses a bounded parent-first plan rather than recursive
  rewrite frames, visiting diamond descendants once. Empty table definitions
  accept PostgreSQL's zero-column syntax without accepting trailing commas.
  Bounded hash joins decode physical and self-describing derived rows at their
  respective boundaries, including external runs, empty builds, and preserved
  LEFT JOIN probes; eligible two-catalog joins no longer require quadratic scans.
  `pg_constraint` and `pg_attrdef` expose PostgreSQL 18 column order and types,
  with system `tableoid` addressable but excluded from star expansion.
  Hash-source decoding includes addressable hidden fields on both sides.
  The shared catalog encoding boundary canonicalizes OID tags, and resolved
  view identities bypass unrelated index enumeration. Reverse relation-OID
  lookup uses validated identity bands and allocates only the rendered name,
  including index names, rather than materializing an index catalog per row.
  Sequence state
  introspection shares one nullable OID parser and honors transaction-visible
  creation and restart state in SELECT records and FROM functions.
  Implicit index identities reserve the complete enforcer
  stride; constraint kinds and partition-trigger clones occupy disjoint OID
  bands. Finite index/trigger generation ranges reject exhaustion before
  installation, including replay, rather than saturating or failing on reads.
- Program length is independent of per-construct limits. Simple-query
  batches, SQL function and procedure bodies, and PL/pgSQL functions,
  procedures, triggers, event triggers, and anonymous blocks retain every
  statement that fits the fixed statement arena. PL/pgSQL local namespaces,
  conditional branches, exception conditions and handlers, and loop-control
  depth use arena-backed full-width representations rather than compiled 64-
  or 255-entry arrays. Named arena exhaustion is atomic. PostgreSQL 18
  differential coverage, allocation-forbidden batches, all procedural hosts,
  and empty-cache recovery qualify programs beyond the former boundary.
- Transaction and statement effect logs do not impose secondary compiled
  widths. `max_ddl_per_transaction` may exceed 256 and continues to size every
  startup-reserved transaction, commit, prepared, apply, and logical-decoding
  structure. Event-trigger command, dropped-object, dependent-drop, and
  deduplication graphs grow geometrically in the fixed statement arena.
  Retry-safe volatile sequence evaluation records every call that fits that
  arena rather than stopping at 1,024. Its buffers grow from the statement
  arena tail and survive front-only per-row scratch rewinds.
  Mutable-routine replay results and encoded pending arguments now follow the
  same persistent-tail contract, removing the 1,024-call array and preventing
  modification retries from reading front storage after rewind. Cursor row
  indexes derive their startup capacity from `cursor_bytes`, so the byte
  budget is the sole cursor-result bound rather than a separate 65,536-row
  ceiling. Logical-replication message indexes grow in fixed work memory and
  no longer inherit the routine-call limit.
  Allocation-forbidden regressions cover 260-table schema cascades and 1,100
  sequence and mutable-routine calls, 1,100 logical messages, and a 70,000-row
  cursor. Checkpoint and empty-cache object recovery preserve the durable
  effects; PostgreSQL 18 differential fixtures cover the former boundaries.
- Effective search paths derive their entry capacity from the accepted GUC
  byte boundary instead of silently stopping at sixteen schemas; resolution,
  `current_schema`, and `current_schemas` share that capacity. Plain and
  regular-expression split table functions count first and materialize their
  exact result in statement memory rather than using a 1,024-row stack array.
  Checkpoint deletions use the startup-sized row overlay as their sole pending
  bound: tables no longer embed an unused 1,024-row tombstone roster or switch
  to a full rewrite at that width. Allocation-forbidden PostgreSQL 18 fixtures
  cross the path and set-result boundaries, while 1,100 deletions remain a
  delta and survive checkpoint publication and empty-cache object recovery.
- XMLTABLE, JSON_TABLE, and publication-introspection result rows use the fixed
  statement arena rather than a shared 256-row array. Flat and nested table
  functions cross the former boundary in PostgreSQL 18 differential tests,
  allocation-forbidden execution, and empty-cache object recovery. Publication
  enumeration excludes the internal large-object relation, including after
  recovery. XML XPath indexing retains its separately documented bound.
- SQL array values admit the durable format's complete 65,535-element count;
  the former 1,024-element stack buffer is not a client-visible limit.
  Literal and binary input, aggregates, concatenation and mutation, split
  functions, variadic arguments, comparisons, `unnest`, JSON/text output,
  casts, indexing, WAL, checkpoints, and object-cold recovery use exact
  statement-arena slices. Sequential consumers decode the payload in one pass
  rather than repeatedly scanning variable-width prefixes. Variadic
  `format()` results grow in that arena rather than stopping at 4 KiB. PostgreSQL 18
  differential and allocation-forbidden regressions cross the old boundary
  and recover the stored values with empty local caches.
- Statement lists are bounded by the statement arena, not the parser's former
  64-item staging arrays or the 256-row VALUES staging: select lists, `IN`
  lists, `ARRAY` constructors, `CASE` arms, function arguments up to
  PostgreSQL's own 100, `DISTINCT ON` / `ORDER BY` / window partition and
  ordering keys, CTEs, set-operation branches, row-locking clauses, `ALTER
  TABLE` actions, `RETURNING` lists, GRANT/REVOKE role lists, VALUES rows, and
  the aggregate, subquery, and correlated-merge execution scratch they feed
  all use geometric arena lists or exact arena slices. Per-row
  correlated-subquery merge scratch is allocated once per statement pass from
  the true node counts, so scans never grow the arena per row. Arena
  exhaustion during parse is SQLSTATE `54000`, not a syntax error. PostgreSQL
  18 differential and allocation-forbidden regressions cross the former
  boundaries and recover stored results with empty local caches.
- JSON values and paths use statement memory for container members, result
  items, accessor chains, subscripts, and rendered text. Parser and executor
  stages report named exhaustion instead of imposing the former 1,024-item,
  256-step, or 64 KiB ceilings. PostgreSQL 18 differential coverage crosses
  these widths and checks JSON versus JSONB function resolution.
- Cluster authorization is startup-sized through independent role,
  membership, role-setting, object-, column-, default-, and parameter-ACL
  capacities. Role-reachability and privilege-cascade scratch use those
  declared bounds without recursion or runtime heap growth. Connection
  counters, PostgreSQL catalogs and information-schema views, WAL,
  checkpoints, loud exhaustion, and empty-cache recovery are qualified above
  the former 64/128/256/512/1,024-entry ceilings.
- Row-level security policies use the independent startup-sized `max_policies`
  catalog, with no additional per-table ceiling. Predicate plans and conjoined
  command gates use statement-arena slices rather than fixed policy arrays.
  A 1,025-policy relation qualifies enforcement, catalog output, named
  exhaustion, checkpoint publication, and empty-cache recovery. Policy role
  lists, routine parameters/results/configuration, and trigger arguments use
  the parser's complete 64-item boundary. Routine signatures and default
  metadata admit those shapes; routine manifest fields stream directly into
  the reserved buffer. `pg_proc` includes TABLE output names, modes, and types
  alongside input parameters, and single-column TABLE result OIDs match
  PostgreSQL. Wide callable definitions, rollback, journal replay,
  checkpoint recovery, and allocation-forbidden execution share regressions.
  PostgreSQL trigger arguments are zero-based and NULL when absent. Routine
  setting values and reset values share one bounded definition, including
  `standard_conforming_strings` and `xmloption`.
  Routine WAL staging borrows images and replay isolates owned decoding;
  the journal event type has a compile-time size bound so wide definitions
  cannot inflate every logical-replication dispatch frame.
  Built-in aggregate groups do not reserve maximum-width custom definitions
  or argument vectors. Custom metadata and direct values use actual-shape
  arena storage; the original cold-PAX 1 MiB query and 8 MiB stack bounds
  remain qualified.
  Procedural statement scratch is isolated from recursive dispatch. DML
  frames do not reserve DDL event graphs, and trigger selection borrows
  metadata until firing. Recursive triggers retain their 16-level named
  exhaustion boundary and original 16 MiB qualification stack.
- Test qualification now owns and removes engine, object-fixture, and
  performance scratch directories. Storage fault injection corrupts only its
  selected preallocated bytes in place, so repeated full and VOPR runs do not
  require whole-file replacement space or accumulate transient databases.

The atomic-transaction capacity audit is implemented: savepoints, deferred
constraint and trigger metadata, retained trigger rows, and statistics undo
have named startup capacities shared by connection, prepared, and apply slots.
Transaction-private table definitions, table statistics, extended-statistics
data, and stored-query dependency images now use startup-sized global pools
keyed to `max_ddl_per_transaction`, `max_analyze_per_transaction`, and the
explicit `max_catalog_versions_per_object`; no object retains a compiled
eight-version inline array. Rules, policies, routines, materialized views, and
view return rules share one contiguous dependency pool governed by
`max_stored_query_dependencies_per_object` (durable maximum 255), rather than
embedding a 64-entry image in each catalog object and pending version. WAL,
checkpoints, template cloning, dependency cascades, catalog reporting, and
execution all consume the same borrowed image contract. Compact backward
chains make latest-version reads, savepoint rollback, commit cleanup, and slot
reuse constant-space operations. Regressions stage more than eight versions of one
object, roll later versions back, commit, publish a checkpoint, discard both
local cache tiers, and verify the surviving catalog state from object storage
with runtime allocation forbidden. A separate regression crosses the former
64-dependency ceiling, proves configured exhaustion is atomic, retries an
ambiguous checkpoint publication, and executes the recovered definition from
empty local caches.
Stored-query dependency planning is likewise catalog-sized. Restrict/cascade
closure, ordered diagnostics, type and column dependency cleanup, schema drops,
and `DROP OWNED` allocate independent statement-arena selections from the
configured table, view, materialized-view, routine, rewrite-rule, operator, and
schema cardinalities instead of sharing a 128-slot array. Selection merges are
bounded by their own catalogs, and dependency depth uses full-width indices.
A regression places views, a SQL routine, and a rewrite rule beyond slot 127,
checks allocation-forbidden cascade rollback and independently sized
`DROP OWNED`, retries ambiguous checkpoint publication, removes both local
cache tiers, and verifies the live graph and its eventual cascade from object
storage across two cold starts.
Deep savepoints preserve GUC, foreign-query, large-object, and cumulative
statistics state without 8-bit nesting or truncated name lists. TRUNCATE closes
inheritance, partition, and foreign-key fan-out over every configured table;
its transaction, journal, logical output, and subscription input paths no
longer impose sixteen-table or 255-relation ceilings. The journal's single
record-kind registry also prevents startup recognition from drifting behind
new encodings. Regressions cover 257 savepoints, 300 deferred trigger firings,
129-table bulk ANALYZE, and 300-table TRUNCATE through rollback, logical apply,
prepared locks, journal replay, checkpoints, and empty-cache recovery.
Both differential harnesses load one capacity fixture; a contract test prevents
local and hosted corpus runs from drifting back to different transaction limits.
Critical cache pressure completes checkpoint publication before further client
dispatch, rather than exhausting a tiny heap while a wide sweep remains paced.
Tests cover repeated small autocommit updates and deferred OLD-row images larger
than the row cache through spill, rollback, validation, and cold recovery.

Geometric GiST/SP-GiST predicate navigation uses immutable bounding-box trees
over small key blocks. All eight built-in geometric classes prune disjoint
objects before exact key and MVCC recheck, including INCLUDE payloads,
transaction rollback, committed overlays, repeated checkpoint publication,
garbage collection, and empty-cache recovery. Construction and traversal
remain fixed-memory; legacy key generations remain readable. Qualification
covers three-level pruning, malformed encodings, PostgreSQL's fuzzy geometry
and non-finite values, and cold reads across every spatial class.
Finite unfiltered GiST/SP-GiST `<-> point` limits use the same trees for
ranked depth-first branch-and-bound traversal. Conservative point-to-box lower
bounds order siblings and a fixed statement-arena max-heap retains only the
`LIMIT + OFFSET` window. Committed overlays seed the cutoff, stale durable
versions receive the normal MVCC rejection, NULLs retain PostgreSQL placement,
and winning key/INCLUDE payloads are rehydrated from remembered leaves.
Residual predicates, row security, locking, ties, unbounded limits, non-finite
origins, legacy rosters, and oversized windows retain complete exact ordering.
Three-level allocation-forbidden traversal, prepared windows, every geometric
operator class, committed and transactional overlays, cold recovery, and
bounded object GET/byte regressions qualify the ranked path.
GIN array, `tsvector`, `jsonb_ops`, and `jsonb_path_ops` generations use
dedicated immutable posting trees. Stable namespace-plus-hash token keys route
array containment/overlap, JSON containment/existence, and exact positive
full-text requirements to 8 KiB leaves; multiple disjunctive tokens are
unioned and row identities deduplicated. Exact SQL and MVCC rechecks remain
authoritative, so token collisions only add candidates. Prefix-only,
negation-only, JSONPath, numeric-only JSON, empty-token, and contained-by cases
decline posting navigation and retain ordinary execution. GiST `tsvector`
continues to use 256-bit token signatures. Empty-cache object-read budgets,
transaction overlays, rollback, repeated publication, recovery, malformed
encodings, PostgreSQL 18 differential cases, and distinct warm/cold performance
workloads qualify all five operator-class paths without runtime allocation.
GiST range, multirange, and network generations and SP-GiST range and network
generations use fixed-size ordered interval summaries in the same immutable
tree. All six built-in range subtypes, empty and unbounded ranges, IPv4, and
IPv6 prune disjoint cold objects while retaining exact SQL and MVCC rechecks.
Unknown network and homogeneous range literals resolve from the indexed
operand at the probe boundary. Three-level cold-read budgets, every physical
operator-class path, transaction overlays, reindexing, checkpoint recovery,
malformed summaries, PostgreSQL 18 differential cases, and distinct warm/cold
performance workloads qualify the implementation without runtime allocation.
External harnesses reserve loopback ports across build delays with atomic,
owner-tracked claims shared by conformance and performance runs. Crashed owners
are reclaimed under an OS lock; startup probes verify fixture process ownership.
Live object-store integration tests own unique directories and remove both
original and cold-recovery caches on scope exit, including assertion failures.
