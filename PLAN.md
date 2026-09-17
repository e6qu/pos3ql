# pos3ql roadmap

Architecture: [README.md](README.md). Naming: [docs/terminology.md](docs/terminology.md). Working rules: [AGENTS.md](AGENTS.md). Externally blocked defects only: [BUGS.md](BUGS.md).

## Product boundary

pos3ql is a PostgreSQL-compatible database whose durable state lives in object
storage. RAM and local disk are bounded, disposable caches.

- Compatibility is defined at PostgreSQL SQL text, v3 wire, catalog, tool, and
  logical-replication boundaries. Unsupported behavior must fail explicitly.
- Durability uses immutable commit batches and checkpoint SSTs published by
  compare-and-swap. PostgreSQL heap pages, physical XLOG, physical streaming
  replication, and binary-WAL tooling are not targets.
- One direct S3-compatible HTTP data-plane implementation serves every object
  store. No provider SDK, custom storage service, translation proxy, or
  provider-specific branch is permitted. The versioned protocol profile and
  multi-implementation qualification suite are the portability boundary.
- Runtime memory is fixed at startup. Execution, caching, sorting, background
  work, and concurrency must remain within named pools and fail loudly on
  exhaustion.
- Third-party PostgreSQL extensions, whether SQL-only or native, are not
  compatibility targets. C shared libraries, the PostgreSQL server ABI, hooks,
  and background workers are also out of scope. The already implemented SQL
  extension package lifecycle remains part of the accepted SQL surface, but no
  further extension qualification or ecosystem work is planned.

## Implemented baseline

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

## Remaining production work

### Remaining bounded scale limits

Replace remaining compile-time per-object inline ceilings with startup-sized
pools or bounded chunked structures where they restrict advertised scale.
Audit their slot widths, journal encodings, checkpoint and manifest structures,
catalog construction, and execution scratch. Exercise maximum-capacity
routine, trigger, policy, transaction, spill, and remaining per-object
structures through checkpoint retry and object-cold recovery while checking
exact startup memory accounting and loud exhaustion.

### Multi-core execution

Remove global query serialization. Use bounded worker-private statement state,
coordinate shared MVCC and locks, and pipeline object I/O while preserving
publication order, group commit, cancellation, fairness, and explicit
backpressure. Demonstrate useful scaling from one core through the supported
worker limit under read-only, write-heavy, and mixed workloads.

### Availability and operations

- Define durable-format compatibility and online/offline migration rules.
- Implement backup, restore, and point-in-time recovery with object-cold tests.
- Add single-writer fencing, ownership leases, safe promotion, and failover.
  Multiple writable processes must never publish concurrently to one prefix.
- Provide health and readiness endpoints, metrics, structured logs, capacity
  reporting, secure credential rotation, packaging, and operational runbooks.

### Performance qualification

Run the reproducible PostgreSQL 18 comparison harness for long durations on
pinned representative hardware against an independently operated compatible
object store. Publish schema-versioned artifacts containing throughput and
latency percentiles, CPU use, fixed-memory occupancy, object requests and
bytes, recovery time, checkpoint interference, replica freshness, and physical
access-path counters.

Qualification must separately cover warm RAM, warm disk, empty local caches,
concurrent writes, checkpoint and compaction pressure, parameterized joins,
large catalogs, and one-through-N logical replicas. CI continues to gate
correctness, memory bounds, request shape, cold recovery, and access-path use;
absolute production performance claims require the representative runs.

## Completion criteria

The production roadmap is complete only when:

- every accepted configuration survives checkpoint and object-cold recovery at
  its declared capacities without truncation;
- concurrent execution scales across multiple cores without weakening MVCC,
  durability, fixed-memory, or backpressure invariants;
- specialized indexes avoid complete-generation reads for their principal
  predicate and nearest-neighbor workloads;
- backup/PITR, format migration, writer fencing, promotion, monitoring,
  credential rotation, packaging, and runbooks are tested end to end; and
- published representative benchmarks support the stated latency, throughput,
  recovery, replica, memory, and object-request claims.
