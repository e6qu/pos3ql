# PostgreSQL 18 compatibility

Compatibility is verified at SQL, SQLSTATE, result shape/value, catalog, and
wire boundaries. The server reports `18.4 (pos3ql 0.1)`; the vendored regression
slice is pinned to PostgreSQL 18.6. Older fixtures retain their exact provenance.
Command classification does not imply support for every grammar production,
planner transformation, function, or PostgreSQL subsystem.

## Implemented boundary

| Area | Accepted surface |
|---|---|
| Protocol | v3.0/3.2 simple and extended queries, prepared statements/portals, text/binary values, COPY, cancellation, TLS, authentication, notices, notifications, and logical replication mode |
| Relational SQL | Tested DDL/DML, MERGE, MVCC transactions, savepoints/two-phase transactions, locks, CTEs, joins, grouping, windows, set operations, views/materialized views, inheritance/partitioning, triggers/rules, cursors, and maintenance |
| Types | Modeled native scalar/array, range/multirange, enum/domain/composite, JSON/JSONB/jsonpath, XML, temporal, network/geometric, full-text, catalog-reference, ACL, transaction, bit/binary string, and UUID families |
| Programming | SQL and PL/pgSQL functions/procedures/triggers, anonymous blocks, set-returning/table functions, dynamic SQL, transition/event triggers, privileges, security-definer state, and configuration scopes |
| Catalogs/tools | Typed PostgreSQL-shaped catalogs and monitoring for tested psql, pg_dump/pg_restore, psycopg, pgJDBC, Npgsql, node-postgres, and pgx paths |
| Replication | PostgreSQL publication/subscription and pgoutput interoperability within the [logical replication boundary](logical-replication.md) |
| Persistence | Immutable object-native commits/checkpoints, recovery, backups, and [versioned durable formats](durable-format.md) |

The source repository's `tests/postgresql18_commands.tsv` records executable
commands and explicit architecture rejections. Curated differential queries,
raw-wire/driver probes, and the vendored regression schedule are continuing
ratchets. Unsupported clauses and type combinations must reject explicitly.

## Physical access paths

| Method | Object-native path |
|---|---|
| Btree | Equality, composite leading-prefix/range probes, parameterized joins and DML; compatible ordered and covering scans |
| Hash | Equality-only single-key probes, including expression/partial, prepared, query/join, and DML paths |
| BRIN | Bitmap equality/range and built-in range/network inclusion; explicit summary maintenance and modeled options |
| GiST | Built-in network, range/multirange, geometry, and full-text predicates; geometric distance ordering |
| GIN | Array, full-text, JSONB, and jsonpath bitmap predicates; conservative posting navigation where a required token exists |
| SP-GiST | Built-in network, range, geometry, locale-independent text ordering/prefix, and geometric distance ordering |

Methods retain catalog and operator-class identity, transaction overlays, WAL,
checkpoint, and cold-recovery behavior. MVCC and SQL rechecks are authoritative.
[Immutable navigation](index-navigation.md) specifies pruning, covering payloads,
legacy formats, and finite geometric nearest-neighbor eligibility. Residual
filters, row security, locking, ties, or unrankable origins retain complete exact
ordering. Expression results are recomputed from fetched rows rather than
projected directly from an index expression tuple. Native PostgreSQL index page
layouts and custom native callbacks are not implemented.

## Capacity boundaries

Named exhaustion is part of the fixed-memory contract. Explicit rejection of a
smaller width does not make that width PostgreSQL-compatible. These principal
boundaries apply through catalogs, wire, WAL, checkpoints, and recovery:

| Kind | Boundary | Source or qualification |
|---|---|---|
| PostgreSQL relation shape | 1,600 columns: tables, views, named composites, record definitions, explicit USING lists | `src/storage/rowenc.rs`; wide relation and record differential fixtures |
| PostgreSQL executable result | 1,664 columns: SELECT targets, routine/table-function results, Describe and Bind formats | Wide result and routine fixtures |
| PostgreSQL statement shapes | 100 routine inputs; 32 index/partition attributes; 4,096 grouping sets; 12 CUBE elements; 31 GROUPING arguments | Parser/type boundaries and accepted/rejected differential cases |
| PostgreSQL wire parameters | 65,535 Parse/Bind and SQL PREPARE/EXECUTE parameters | Fixed prepared/portal buffers plus statement memory; maximum-width wire fixture |
| Durable array count | 65,535 elements; exact slices or streaming traversal | Array text/binary, execution, persistence, and cold-recovery fixtures |
| Durable constraint identity | 64 constraints per modeled table kind and 64 domain checks | Manifest v14 and 64-position catalog/trigger OID stride; SQLSTATE 54000 on the next item |
| Other definition breadth | Documented 64-item constructs, including LIST bounds, inheritance parents, trigger arguments, and routine configuration entries | Shared parse/storage boundary; widening requires review of representation and identity |
| Startup capacities | Independent catalog, connection, transaction/savepoint, row-version, lock, replication, cache, and checkpoint pools | Configuration and exact startup memory plan; named errors before partial publication |
| Statement memory | Lists, joins, programs, volatile/routine retry logs, event graphs, JSON widths, split/table-function rows, and variable-width geometry | Fixed arenas; arena exhaustion is a program-limit error |
| Value-specific limits | Full-text, XML/XPath, JSON/path nesting, rendered values, GUC bytes, and finite catalog identities | Typed source boundaries and specialized contracts below |

Startup-sized catalogs have independent capacities; table count does not silently
size unrelated classes. Transaction bounds also cover prepared and subscription
slots. `max_catalog_versions_per_object` bounds retained definition/undo versions.
`checkpoint_manifest_bytes`, live-block, replay, merge, garbage-batch, and backup
rosters are separate reservations. Deletion batches limit work per beat, not the
number of objects cleanup may ultimately process.

Wire and durable identity widths are validated at startup and recovery. Arrays,
programs, statement lists, multiranges, and mutable-routine results have no
additional former 64/256/1,024-item staging ceiling. Effective search paths use
the accepted GUC byte representation; cursor indexes use `cursor_bytes`.
JSON/path and XML/XPath limits are specified in [SQL/JSON](sql-json.md) and
[SQL/XML](sql-xml.md). Inspect the memory plan for the chosen configuration;
accepted width can still exhaust its named statement or startup budget.

## Architecture boundaries and remaining gaps

- Query execution is still serial. Workspace and catalog synchronization
  prepare for the [remaining concurrency work](../PLAN.md#remaining-sequence).
- PostgreSQL's cost model and exact EXPLAIN text are not compatibility complete.
  PostgreSQL parallel-query/JIT and planner/executor hooks do not exist.
- Heap layout, heap-version `ctid`, HOT, physical vacuum internals, physical
  XLOG/streaming replication, hot standby, and binary-WAL tools are non-goals.
  Object-native row identities are not exposed as invented heap addresses.
- Logical copies are asynchronous independently durable databases; transparent
  shared-storage replicas and active-active writers are not implemented.
- Monitoring fields for absent PostgreSQL subsystems use documented truthful
  empty/zero states; real modeled activity must not be accepted and ignored.
- Smaller definition and XPath subsets remain documented scale/surface
  differences. Incompatible format changes need a verified offline migration.
- Regression schedule omissions must remain explicit and reviewable, rather
  than being hidden behind an expected-failure budget.

## Extensions

The SQL-extension package lifecycle is implemented: control/version scripts,
updates, dependencies, trusted installation, relocation, membership, ownership,
comments, catalogs, transactions, dump/restore, and durable recovery.
The repository's fixture packages qualify that lifecycle.

Third-party SQL-only or native extensions are not compatibility targets or
certified. PostgreSQL C libraries, server ABI/hooks, background workers, native
procedural languages, foreign-data wrappers, and custom index callbacks are
outside scope. No native or sandbox extension ABI is planned.
