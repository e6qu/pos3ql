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
  LIST bounds use the complete 64-value parser list. Constraint enforcement,
  record shapes, PostgreSQL catalogs, WAL, checkpoint manifests, loud
  over-boundary errors, and empty-cache object recovery share those limits.
  New index WAL uses 32-bit masks while legacy 8-bit WAL remains readable;
  textual checkpoint masks remain backward-readable.
  Wide table/domain WAL staging borrows definitions, decoder branches isolate
  fixed scratch, and manifest replay transfers pending table ownership at one
  choke point rather than reserving a copy in every branch. Schema-only catalog
  resolution and procedural count passes recycle temporary query state.
  Inherited ALTER uses a bounded parent-first plan rather than recursive
  rewrite frames, visiting diamond descendants once. Empty table definitions
  accept PostgreSQL's zero-column syntax without accepting trailing commas.
  Bounded hash joins decode physical and self-describing derived rows at their
  respective boundaries, including external runs, empty builds, and preserved
  LEFT JOIN probes; eligible two-catalog joins no longer require quadratic scans.
  `pg_constraint` and `pg_attrdef` expose PostgreSQL 18 column order and types,
  with system `tableoid` addressable but excluded from star expansion.
  The shared catalog encoding boundary canonicalizes OID tags. Sequence state
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
- Test qualification now owns and removes engine, object-fixture, and
  performance scratch directories. Storage fault injection corrupts only its
  selected preallocated bytes in place, so repeated full and VOPR runs do not
  require whole-file replacement space or accumulate transient databases.

## Remaining production work

### Remaining bounded scale limits

Replace remaining compile-time per-object inline ceilings with startup-sized
pools or bounded chunked structures where they restrict advertised scale.
Audit their slot widths, journal encodings, checkpoint and manifest structures,
catalog construction, compaction, and garbage collection. Exercise maximum-
capacity inheritance, routine, trigger, policy, transaction, spill, compaction,
garbage collection, checkpoint retry, and object-cold recovery while checking exact
startup memory accounting and loud exhaustion.

### Multi-core execution

Remove global query serialization. Use bounded worker-private statement state,
coordinate shared MVCC and locks, and pipeline object I/O while preserving
publication order, group commit, cancellation, fairness, and explicit
backpressure. Demonstrate useful scaling from one core through the supported
worker limit under read-only, write-heavy, and mixed workloads.

### Navigable specialized indexes

Replace complete immutable-generation walks with object-native GIN posting
structures and navigable GiST/SP-GiST nodes. Predicate and nearest-neighbor
limits must prune object reads without adopting PostgreSQL page layouts or
native operator-class callbacks.

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
