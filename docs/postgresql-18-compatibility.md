# PostgreSQL 18 compatibility

PostgreSQL compatibility is a tested client boundary, not a claim that pos3ql
contains PostgreSQL's server internals. The compatibility major is PostgreSQL
18; the server currently reports 18.4, while the newly vendored upstream
regression slice is pinned to 18.6 so stable-branch changes are tested rather
than silently inherited from a developer machine. Older 18.4-derived fixtures
retain their exact provenance. The command inventory, curated differential
corpus, driver probes, and vendored upstream regression slices are ratchets:
support exists only where
the same operation returns compatible SQL, SQLSTATE, row shape, value, catalog
metadata, and wire representation.

## Implemented boundary

| Area | Implemented |
|---|---|
| Client protocol | PostgreSQL v3 simple and extended query flow, prepared statements and portals, text and binary parameters/results, COPY, cancellation, TLS, authentication, notices, notifications, and logical-replication mode. |
| SQL | The PostgreSQL 18 top-level command inventory has either an executable implementation or a tested architecture rejection. Implemented relational execution includes DDL/DML, `MERGE`, transactions and two-phase transactions, CTEs, joins, grouping, windows, set operations, views, materialized views, inheritance and partitioning, triggers, rules, cursors, and maintenance commands within their tested forms. This is not a claim that every PostgreSQL grammar production or planner transformation is implemented. |
| Types and expressions | The native scalar, array, range, multirange, enum, domain, composite, JSON/JSONB/jsonpath, XML, temporal, network, geometric, catalog-reference, ACL, transaction, bit-string, binary-string, UUID, and full-text families documented in this repository, including their modeled operators, functions, aggregates, casts, COPY, wire, indexes, WAL, checkpoint, and recovery paths. |
| Server programming | SQL functions, PL/pgSQL functions/procedures/triggers/anonymous blocks, set-returning and table functions, dynamic SQL, transition tables, event triggers, privileges, security-definer state, and configuration scopes within the bounded executor. |
| Catalogs and tools | PostgreSQL 18-shaped catalogs and monitoring views required by the tested psql, pg_dump/pg_restore, psycopg, pgJDBC, Npgsql, node-postgres, and pgx paths. Catalog rows describe pos3ql's real modeled objects; absent PostgreSQL subsystems expose documented empty/zero states where that is truthful. |
| Replication | PostgreSQL 18 logical publication/subscription and pgoutput v1-v4 at the documented object-native boundary, including interoperability with PostgreSQL publishers, subscribers, and `pg_recvlogical`. |
| Durability | Object-native immutable commit/checkpoint data and compare-and-swap publication through the versioned S3-compatible profile. PostgreSQL heap pages and physical XLOG are not used. |

The exact top-level command disposition lives in
[`tests/postgresql18_commands.tsv`](../tests/postgresql18_commands.tsv). The
logical-replication boundary is specified separately in
[`logical-replication.md`](logical-replication.md).

## Deliberate and current limits

Atomic transaction capacity is configurable at startup: savepoint count,
deferred constraint/trigger metadata, retained trigger row bytes, and ANALYZE
undo entries. Exhaustion reports a program-limit error rather than growing
runtime memory. TRUNCATE includes inheritance and partition descendants and
foreign-key cascade closure, with table lists bounded by configured physical
table capacity across SQL, prepared transactions, durability, and pgoutput.
The differential corpus checks these SQL semantics against PostgreSQL 18.6;
fixed-allocation and object-cold tests exercise larger declared capacities.

- PostgreSQL heap layout, page identifiers, `ctid` semantics, HOT, vacuum's
  physical implementation, physical XLOG, physical streaming replication,
  hot standby, and binary-WAL tooling are not targets.
- Modeled BRIN indexes physically execute bitmap equality/range scans and every built-in range/network inclusion strategy over
  durable object-block indexes for plain, multicolumn, expression, partial,
  prepared, join, and DML paths. PostgreSQL 18's built-in BRIN operator
  classes, families, strategy operators, support procedures, and relation
  options retain exact catalog and recovery identities. Minmax-multi and Bloom
  parameters plus explicit summarize/desummarize maintenance retain PostgreSQL
  catalog, rollback, WAL, checkpoint, and cold-recovery behavior. Modeled hash indexes
  physically execute their PostgreSQL equality-only single-key
  boundary for plain, expression, partial, prepared, query, join, and DML
  probes. Their method and built-in operator-class identities persist through
  catalogs, WAL, checkpoints, copied and partitioned indexes, reindexing, and
  object-cold recovery. Modeled btree indexes physically execute complete
  single/composite and expression equality probes (including prepared
  parameters) and equality-leading
  prefixes with lower, upper, or two-sided bounds on the following column for queries and direct
  UPDATE/DELETE target scans. Exact resident probes use the
  complete startup-bounded hash map; durable equality probes use per-block
  filters. Checkpoints externally sort durable keys using PostgreSQL type and
  collation semantics, and prefix/range probes seek by immutable per-block key
  bounds. Compatible `ORDER BY` clauses use forward/backward compact-key
  ordering with equality-fixed prefixes and exact PostgreSQL NULL placement;
  key- or `INCLUDE`-covered projections execute as index-only scans from
  versioned immutable payloads, including cold recovery and committed
  post-checkpoint overlays. Nested-loop joins and
  joined UPDATE/DELETE sources parameterize exact or range keys from rows
  already bound on their outer side. Partial indexes have distinct filtered
  generations and are selected only when the query conservatively implies the
  stored predicate; expression and partial keys also support compatible
  ordered traversal. Expression-key results themselves are recomputed from a
  fetched row rather than projected directly from an index tuple.
- Modeled GiST indexes physically scan bounded immutable encoded-key
  generations for supported network containment, planar-geometric
  relationships, range/multirange relationships, and `tsvector`/`tsquery`
  predicates. Geometric keys use object-native bounding-box nodes to prune
  disjoint subtrees. Each selected key is evaluated exactly before its row
  identity becomes a candidate. The nine PostgreSQL 18 built-in classes,
  nine families, 100
  strategy rows, 68 support rows, `tsvector` `siglen`, relation
  `fillfactor`/`buffering`, included columns, DML maintenance, WAL,
  checkpoints, and object-cold recovery share one typed boundary. Built-in
  point, box, polygon, and circle classes execute `<-> point`
  K-nearest-neighbor ordering over compact immutable keys, including prepared
  origins, filters, `LIMIT`, covering scans, overlays, and cold recovery.
  PostgreSQL GiST tree pages and native/custom operator classes are not
  implemented.
- Modeled GIN indexes physically execute array containment/overlap,
  `tsvector` search, JSONB containment/existence, and jsonpath predicates as
  bitmap plans. Array, full-text, `jsonb_ops`, and `jsonb_path_ops` values are
  extracted into dedicated object-native posting trees; exact SQL and MVCC
  rechecks make token-hash collisions conservative. Modeled SP-GiST indexes
  physically execute network, range, box, point, polygon, locale-independent
  text-order, and prefix predicates.
  PostgreSQL 18.6's four GIN and seven SP-GiST classes, 11 families, 90
  strategy rows, 56 support rows, method-specific DDL/options, cloning,
  partition children, reindexing, DML, WAL, checkpoints, and object-cold
  recovery share one typed boundary. Built-in point, box, and polygon SP-GiST
  classes execute the same `<-> point` K-nearest-neighbor boundary.
  Geometric predicate navigation uses the same immutable bounding-box tree
  boundary, including both point classes, box and polygon. Network, range,
  full-text, GIN posting, and ranked nearest-neighbor navigation are
  implemented. Finite unfiltered limits prune by conservative node distance;
  residual filters, row security, locking, ties, and unbounded orders retain
  complete exact ordering. PostgreSQL page layout and posting-list format, and
  native/custom callbacks are not implemented.
- PostgreSQL's cost model and exact `EXPLAIN` plan text are not compatibility
  complete. Parallel query, JIT, and PostgreSQL planner/executor hooks do not
  exist. Query execution is currently serialized through one server process.
- Tables, indexes, ordinary and materialized views, routines, casts, operators,
  operator families/classes, triggers, row-level security policies, publications, collations, conversions,
  text-search objects, event triggers, tablespaces, and comments use independent
  startup-sized pools. Their configured exhaustion is a loud program-limit
  error; object-cold recovery preserves catalogs larger than their former
  fixed or table-derived limits. Checkpoint bookkeeping covers every configured
  physical-table slot, including the internal large-object table. The complete
  checkpoint catalog image has a named startup-reserved
  `checkpoint_manifest_bytes` bound and fails before publication if it is full.
  Policies use `max_policies` (default 256) without a second per-table ceiling;
  their predicates are statement-arena bounded. Routine parameters, output
  columns, configuration settings, trigger arguments, and policy roles accept
  the complete 64-item parser boundary. This remains below PostgreSQL's
  100-input-argument routine limit. `RETURNS TABLE` catalog argument metadata
  includes the independently bounded input and output shapes. Trigger arguments
  are zero-based and NULL when absent, matching PostgreSQL.
  Database and schema catalogs, connection counters, statistics, cloning, and
  publication membership are also startup-sized and survive object-cold
  recovery above their former 32-slot limits. Catalog builders for
  startup-sized objects allocate row references from the fixed statement arena
  according to transaction-visible cardinality. Remaining per-object inline
  bounds and the documented catalog-identity widths still limit accepted scale.
- Index tuples and partition keys enforce PostgreSQL 18's exact 32-attribute
  boundaries. The current bounded parser and row format support 64 table
  constraints of each modeled kind, 64 domain checks, 64 named-composite
  fields, and 64 LIST-bound values. Those accepted widths are enforced and
  preserved through record typing, catalogs, WAL, checkpoints, and object-cold
  recovery; wider PostgreSQL tables, composites, constraint collections, and
  LIST bounds remain loud program-limit errors rather than partial objects.
- The bounded statement boundary is 64 Bind parameters, CTEs, row-locking
  clauses, named windows, `ALTER TABLE` actions, `JOIN ... USING` columns,
  aggregate calls, scalar subqueries, set-operation leaves, and independent
  anonymous record shapes. Partition and ordering lists in one window may each
  contain 64 expressions. Static description, execution, and `EXPLAIN` accept
  the same maximum shapes; the plan tree uses the fixed statement arena.
  Item 65 returns SQLSTATE `54000`, and maximum shapes are exercised before
  and after empty-cache object recovery.
- Program length does not share that 64-item arity limit. Simple-protocol
  batches, SQL-language bodies, and PL/pgSQL bodies are sized from their source
  in the fixed statement arena. PL/pgSQL locals, branches, exception handlers
  and conditions, and nested loop-control state are arena-backed. Arena
  exhaustion is reported before a simple batch executes, while accepted stored
  functions, procedures, triggers, event triggers, and anonymous blocks retain
  their complete programs across WAL, checkpoints, and object-cold recovery.
- Event-trigger command and dropped-object graphs are statement-arena bounded,
  including dependent objects and duplicate suppression; they have no separate
  256-object limit. `max_ddl_per_transaction` is independently startup-sized
  and may exceed 256. Retry-safe `nextval`, `currval`, `lastval`, and `setval`
  effects likewise use statement memory rather than a 1,024-call array. The
  persistent log grows from the arena tail and survives front-only executor
  scratch rewinds. The accepted widths are exercised against PostgreSQL 18 and
  through empty-cache object recovery; statement-memory exhaustion remains
  SQLSTATE `54000`.
- Compatibility is not universal merely because all top-level command names
  are classified. Unsupported clauses, type combinations, functions, catalog
  objects, and physical assumptions must return explicit errors.
- The vendored PostgreSQL suite deliberately selects statement-boundary ranges.
  Omitted ranges depend on an architecture listed above or on a known
  unsupported input family; the executable manifest makes every omission
  reviewable instead of converting it into an expected-failure budget.

## Extensions are not a target

pos3ql already implements PostgreSQL's SQL-extension package lifecycle:
control files, versioned SQL scripts and update paths, dependencies, trusted
installation, schema selection and relocation, extension membership,
configuration-table dump metadata, ownership, comments, catalogs,
transactions, WAL, checkpoints, dump/restore, and object-store recovery. This
remains accepted SQL behavior, not a commitment to PostgreSQL's extension
ecosystem.

Third-party PostgreSQL extensions, including SQL-only packages, are not a
compatibility target and are not certified. The repository's `pos3ql_base` and
`pos3ql_ext` packages are conformance fixtures, not user extensions. pos3ql
does not load PostgreSQL C shared libraries and does not implement PostgreSQL's
server ABI, hooks, background workers, custom native types, native procedural-
language handlers, native foreign-data wrappers, or native index access-method
callbacks. No native or sandbox extension ABI is planned.
