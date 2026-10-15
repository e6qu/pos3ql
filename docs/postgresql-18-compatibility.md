# PostgreSQL 18 compatibility

Use this guide before assuming that a PostgreSQL application or tool works
unchanged. We compare SQL results, error codes, catalog metadata, and messages
sent to clients with actual PostgreSQL. Broad support does not mean every
PostgreSQL feature, query shape, or third-party extension is implemented.

**User example:** `psql`, a driver, and `pg_restore` can exercise the tested
paths. A PostgreSQL extension containing a C library cannot be installed just
because its SQL installation command is recognized. Unsupported behavior must
fail explicitly.

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

### Partition column order

An existing table attached as a partition must currently store its columns in
the parent's order. Reordered columns return `0A000`; PostgreSQL accepts those
layouts. Ordinary inheritance supports matching columns in different orders.
For example, attaching `(amount integer, id integer)` beneath a partitioned
parent `(id integer, amount integer)` is not yet supported.

## Capacity boundaries

Some limits come from PostgreSQL, some are smaller implementation limits, and
some are reservations you choose in the startup configuration. These are
different promises. A wide query can be a supported SQL shape yet still need
more statement memory than you reserved.

**User example:** adding a 65th constraint of a modeled kind fails with
SQLSTATE `54000` even if memory remains available. Filling `max_tables` also
fails, but that reservation can be changed before the next startup. Increasing
a startup reservation does not remove a durable-format limit.

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
| Executing snapshot retention | 1,024 owned statement snapshot slots per query workspace; separate transaction snapshots; checked 64-bit lease identities | Startup-owned registry; SQLSTATE 54000 before partial registration or identity reuse |
| Query SST root retention | 64 source slots per query workspace; configured generation capacity per slot; shared unique-root capacity twice the startup table-slot × generation capacity | Startup-owned roots, schema, and reader buffers; scans with no resident overlay remain usable after table/Storage retirement; SQLSTATE 54000 before partial capture |
| Object-row source retention | 64 point-source slots per query workspace, separate from scan sources; checked shared lease counts | Complete captured generations and newest deletion metadata; SQLSTATE 54000 on exhaustion, without reducing scan capacity |
| Retained row metadata | 64 copied point reads per query workspace; each reserves the configured maximum pending and committed versions | Startup-owned metadata, heap sources, and saved-file roots; SQLSTATE 54000 on exhaustion, with no shared version lock held during retention; captured-table admission returns 40001 before reserving a slot when its identity changed |
| Bound prepared statements | Existing `max_prepared` statement slots and `prepared_bytes` per slot | Releasing a name preserves bytes needed by bound portals; slots are reusable after the last portal closes; exhaustion remains SQLSTATE 54000 |
| DML output schema | Shared startup schema-image pool: table slots × (`max_catalog_versions_per_object` + 1) × `query_workspace_slots`; released after output analysis | SQLSTATE 54000 before partial description on exhaustion; descriptions avoid full schema copies in the statement arena |
| Reader-history cleanup | One separate startup workspace with `max(table_rows, large_object_pages)` 64-bit row identities; at most 64 identities per shared-lock transition | Included in startup memory; does not consume query walk slots; current reader retention is rechecked for every batch |
| Retained row identities | 64 internal walk slots per query workspace; each reserves the largest configured table, large-object page, or value-index row capacity | Owned startup slots survive map/table/Storage removal; SQLSTATE 54000 on slot or identity-buffer exhaustion |
| Deferred row retention for sorted scans and enum rewrites | 32 bytes per optional row record on the release target, within the fixed statement arena; one shared source per table | Records borrow their original heap/file owner; statement or source exhaustion returns SQLSTATE 54000 |
| Sequential SQL resident scans | 32 bytes per resident overlay row, including deletions, in the fixed statement arena; saved rows continue streaming | Captured visible versions and original heap/files survive cleanup and table/Storage removal; SQLSTATE 54000 releases failed capture ownership |
| Table COPY TO retention | 32 bytes per ordered output record on the release target, plus captured resident records and saved-file lists in the fixed statement arena | The named relation's original source releases on completion or error; SQLSTATE 54000 on exhaustion, without raising query limits |
| Index validation retention | 32 bytes per resident overlay row plus saved-file lists in the fixed statement arena, reused between target tables; per-row expression scratch is recycled; saved rows stream | Ordinary, copied, and partition indexes include own changes and current committed rows; SQLSTATE 54000 releases failed creation and its pending catalog entries |
| ANALYZE resident retention | 32 bytes per resident overlay row plus saved-file lists in the existing work arena; saved rows stream | SQLSTATE 54000 on exhaustion; failed capture releases original data owners and restores the query snapshot |
| Schema validation retention | 32 bytes per resident overlay row plus saved-file lists in the fixed statement arena, reused between tables; saved rows stream | Current committed and own changes are checked independently of query snapshots; SQLSTATE 54000 releases failed capture ownership |
| ON CONFLICT row retention | Only the matched encoded row is copied into the existing statement arena; per-row index expression scratch is reused | Current committed and own changes select the conflict; SQLSTATE 54000 on capture exhaustion leaves rows unchanged and releases the source |
| Local UPDATE and DELETE retention | Matched encoded bytes, 32-byte row records, and per-table column maps, schemas, and parsed checks/generated expressions within the existing statement arena | Sources release after copying; SQLSTATE 54000 aborts failed capture before mutation; identity-only DELETE avoids unnecessary byte copies |
| Internal transaction identities | Checked 32-bit transaction identities and per-transaction command counters | SQLSTATE 54000 before identity reuse or command advancement |
| Statement memory | Retained heap/spilled row images, escaped query metadata, lists, joins, programs, retry logs, event graphs, JSON widths, table-function rows, and variable-width geometry | Fixed arenas; arena exhaustion is a program-limit error |
| Value-specific limits | Full-text, XML/XPath, JSON/path nesting, rendered values, GUC bytes, and finite catalog identities | Typed source boundaries and specialized contracts below |

Startup-sized catalogs have independent capacities; table count does not silently
size unrelated classes. Transaction bounds also cover prepared and subscription
slots. The active transaction identity registry reserves
`max_connections + max_prepared_transactions` entries. Creating a transaction
returns `54000` if none is free; existing transactions remain usable, and ending
one releases its entry. SQL and maintenance share one checked identity counter.
`max_catalog_versions_per_object` bounds retained definition/undo versions.
`max_row_versions_per_row` bounds pending writes and committed snapshot versions;
each write retains an immutable image, including writes in the same command.
Pending version identity exhaustion is a named program-limit error.
Statistics journal receipts reserve one entry per physical table in each query
workspace. They retain the captured relation-count revision through staging;
full or already retained workspaces and revision exhaustion return `54000`.
`checkpoint_manifest_bytes`, live-block, replay, merge, garbage-batch, and backup
rosters are separate reservations. Deletion batches limit work per beat, not the
number of objects cleanup may ultimately process.

Wire and durable identity widths are validated at startup and recovery. Arrays,
programs, statement lists, multiranges, and mutable-routine results use their
fixed memory budgets without separate staging-count ceilings. Heap locations
use 32-bit addresses; `memtable_bytes` must not exceed `u32::MAX`. Startup reserves
two heap buffers of that size, including one for relocation while readers retain
old bytes. Reclamation returns `54000` while both buffers are retained. Effective search paths use
the accepted GUC byte representation; cursor indexes use `cursor_bytes`.
JSON/path and XML/XPath limits are specified in [SQL/JSON](sql-json.md) and
[SQL/XML](sql-xml.md). Inspect the memory plan for the chosen configuration;
accepted width can still exhaust its named statement or startup budget.

## Architecture boundaries and remaining gaps

**User example:** an application can receive logically replicated order
changes, but a PostgreSQL physical standby cannot replay pos3ql's journal.
Similarly, familiar index names describe SQL behavior; they do not imply
PostgreSQL's on-disk page layout.

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
