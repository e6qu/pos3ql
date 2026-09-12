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

- PostgreSQL heap layout, page identifiers, `ctid` semantics, HOT, vacuum's
  physical implementation, physical XLOG, physical streaming replication,
  hot standby, and binary-WAL tooling are not targets.
- Hash, GiST, GIN, SP-GiST, and BRIN index execution are not implemented.
  Plain-column btree indexes physically execute complete single/composite
  equality probes (including prepared parameters) and equality-leading
  prefixes with lower, upper, or two-sided bounds on the following column for queries and direct
  UPDATE/DELETE target scans. Exact resident probes use the
  complete startup-bounded hash map; durable equality probes use per-block
  filters. Checkpoints externally sort durable keys using PostgreSQL type and
  collation semantics, and prefix/range probes seek by immutable per-block key
  bounds. Ordered result traversal and ORDER BY satisfaction,
  join-parameterized scans, expression and partial matching, and
  index-only scans remain production work.
- PostgreSQL's cost model and exact `EXPLAIN` plan text are not compatibility
  complete. Parallel query, JIT, and PostgreSQL planner/executor hooks do not
  exist. Query execution is currently serialized through one server process.
- Compatibility is not universal merely because all top-level command names
  are classified. Unsupported clauses, type combinations, functions, catalog
  objects, and physical assumptions must return explicit errors.
- The vendored PostgreSQL suite deliberately selects statement-boundary ranges.
  Omitted ranges depend on an architecture listed above or on a known
  unsupported input family; the executable manifest makes every omission
  reviewable instead of converting it into an expected-failure budget.

## Extensions

pos3ql implements PostgreSQL's SQL-extension package lifecycle: control files,
versioned SQL scripts and update paths, dependencies, trusted installation,
schema selection and relocation, extension membership, configuration-table
dump metadata, ownership, comments, catalogs, transactions, WAL, checkpoints,
dump/restore, and object-store recovery. An SQL-only extension can work when
every object and statement in its install/update scripts is itself in the
implemented boundary.

No third-party PostgreSQL extension is currently certified. The repository's
`pos3ql_base` and `pos3ql_ext` packages are conformance fixtures, not user
extensions. In particular, pos3ql does not load PostgreSQL C shared libraries
and does not implement PostgreSQL's server ABI, hooks, background workers,
custom native types, native procedural-language handlers, native foreign-data
wrappers, or native index access-method callbacks. Extensions requiring those
facilities cannot run unchanged. They require a native pos3ql implementation
or a future bounded and sandboxed extension mechanism.

Qualification of real SQL-only extensions, with pinned upstream source and
their installcheck suites, remains required before naming any extension as
supported.
