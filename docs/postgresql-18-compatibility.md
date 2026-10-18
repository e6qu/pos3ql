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

Arrays of domains over built-in arrays are supported. For example, a domain
`integer_vector` over `integer[]` can be used as `integer_vector[]`.
Arrays of domains whose base is an array of user-defined values are rejected:
the stored base code cannot retain that nested declaration. A scalar domain
containing an array of scalar domains is supported.
For example, `CREATE DOMAIN positive_ids AS positive_integer[]` is distinct
from an unsupported `positive_ids[]` column.

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

An attached table can store matching columns in a different order. SQL
expressions and `RETURNING` follow the table named in the command; stored rows,
row triggers, generated values, and constraints follow the destination table.

For example, this inserts `(1, 30)` into a child that stores `amount` first:

```sql
CREATE TABLE orders (id integer, amount integer) PARTITION BY RANGE (id);
CREATE TABLE early_orders (amount integer, id integer);
ALTER TABLE orders ATTACH PARTITION early_orders FOR VALUES FROM (0) TO (10);
INSERT INTO orders VALUES (1, 30) RETURNING id, amount;
```

Defaults come from the table named in the command. Stored generated columns
use the destination table's expression after its BEFORE row triggers. A
generated column cannot be a partition key (`42P17`). `ON CONFLICT DO UPDATE`
cannot move the conflicting row to another partition (`0A000`).

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
| Durable constraint identity | 64 constraints per modeled table kind and 64 domain checks | Current manifest and 64-position catalog/trigger OID stride; SQLSTATE 54000 on the next item |
| Other definition breadth | Documented 64-item constructs, including LIST bounds, inheritance parents, trigger arguments, and routine configuration entries | Shared parse/storage boundary; widening requires review of representation and identity |
| Stored default expressions | 120 bytes, including enum member bindings and saved catalog display text | Existing startup reservation; SQLSTATE 54000 names an overfull default or bound enum expression before installation |
| Startup capacities | Independent catalog, connection, transaction/savepoint, row-version, lock, replication, cache, and checkpoint pools | Configuration and exact startup memory plan; named errors before partial publication |
| Durable object requests | `object_store_get_slots` read clients, one separate atomic block read/write client, and one metadata/WAL client | Each reserves `object_store_head_bytes` + `object_store_response_bytes`; read-slot controls and one 256 KiB block scratch buffer are also charged at startup, separately from cache tiers; atomic reads preserve pending reader requests but can wait synchronously or duplicate a GET |
| Executing snapshot retention | 1,024 owned statement snapshot and selected row/index file owner slots per query workspace; separate transaction snapshots; checked 64-bit identities; independent `2 * max_value_indexes` retained index graphs | File ownership survives scan/arena release and statement waits; startup membership bits deduplicate aliases without using point-source slots; SQLSTATE 54000 before partial registration, file capture or identity reuse |
| Query SST root retention | 64 source slots per query workspace; configured generation capacity per slot; shared unique-root capacity twice the startup table-slot × generation capacity | Startup-owned roots, schema, and reader buffers; scans with no resident overlay remain usable after table/Storage retirement; SQLSTATE 54000 before partial capture |
| Object-row source retention | 64 point-source slots per query workspace, separate from scan sources; checked shared lease counts | Complete captured generations and newest deletion metadata; SQLSTATE 54000 on exhaustion, without reducing scan capacity |
| Retained row metadata | 64 copied point reads per query workspace; each reserves the configured maximum pending and committed versions | Startup-owned metadata, heap sources, and saved-file roots; SQLSTATE 54000 on exhaustion, with no shared version lock held during retention; captured-table admission returns 40001 before reserving a slot when its identity changed |
| Bound prepared statements | Existing `max_prepared` statement slots and `prepared_bytes` per slot | Releasing a name preserves bytes needed by bound portals; slots are reusable after the last portal closes; exhaustion remains SQLSTATE 54000 |
| Streamed COPY statement-trigger inputs | `txn_rows` records and `copy_transition_bytes` (256 KiB default) per connection and subscription worker | Startup-owned encoded values; no later source lookup; SQLSTATE 54000 aborts COPY on row/byte exhaustion; cleanup permits a smaller retry |
| DML output schema | Shared startup schema-image pool: table slots × (`max_catalog_versions_per_object` + 1) × `query_workspace_slots`; released after output analysis | SQLSTATE 54000 before partial description on exhaustion; descriptions avoid full schema copies in the statement arena |
| Resident row metadata | `table_rows` stable row cells per user table, `large_object_pages` for the internal page table; each has a fixed row-identity lookup index with at least eight buckets and at most one-half load | Both arrays and inline controls are charged at startup; a row update at full capacity replaces its cell, while a new row fails with SQLSTATE 54000; lookup backshifts never move another row cell |
| Reader-history cleanup | Fixed cursor, retry and pressure-scan controls; at most 64 stable slot visits per ordinary maintenance turn; no table-sized identity buffer or query walk slot | Normal statement/transaction completion schedules work, including idle/download waits; explicit synchronous completion is available. Every batch rechecks readers; owner contention retries after one second without changing readiness. Later releases cannot be erased by a partial pass |
| Heap publication ownership | One optional row-map write guard per physical table slot (`max_tables + 1`), with aligned arena storage and exact mutex/arena controls | Charged at startup; partial acquisition releases all acquired maps and version ownership before waiting; SQLSTATE 54000 names an undersized ownership workspace before publication |
| Retained row identities | 64 internal walk slots per query workspace; each reserves the largest configured table, large-object page, or value-index row capacity | Owned startup slots survive map/table/Storage removal; SQLSTATE 54000 on slot or identity-buffer exhaustion |
| Deferred row retention for sorted scans | 32 bytes per optional row record on the release target, within the fixed statement arena; one shared source per table | Records borrow their original heap/file owner; statement or source exhaustion returns SQLSTATE 54000 |
| Sequential SQL resident scans | 32 bytes per resident overlay row, including deletions, in the fixed statement arena; saved rows continue streaming | Captured visible versions and original heap/files survive cleanup and table/Storage removal; SQLSTATE 54000 releases failed capture ownership |
| Table COPY TO retention | 32 bytes per ordered output record on the release target, plus captured resident records and saved-file lists in the fixed statement arena | The named relation's original source releases on completion or error; SQLSTATE 54000 on exhaustion, without raising query limits |
| Index validation retention | 32 bytes per resident overlay row plus saved-file lists in the fixed statement arena, reused between target tables; per-row expression scratch is recycled; saved rows stream | Ordinary, copied, and partition indexes include own changes and current committed rows; SQLSTATE 54000 releases failed creation and its pending catalog entries |
| ANALYZE resident retention | 32 bytes per resident overlay row plus saved-file lists in the existing work arena; saved rows stream | SQLSTATE 54000 on exhaustion; failed capture releases original data owners and restores the query snapshot |
| Schema validation retention | 32 bytes per resident overlay row plus saved-file lists in the fixed statement arena, reused between tables; saved rows stream | Current committed and own changes are checked independently of query snapshots; SQLSTATE 54000 releases failed capture ownership |
| ON CONFLICT row retention | Only the matched encoded row is copied into the existing statement arena; per-row index expression scratch is reused | Current committed and own changes select the conflict; SQLSTATE 54000 on capture exhaustion leaves rows unchanged and releases the source |
| Local UPDATE and DELETE retention | Matched encoded bytes, 32-byte row records, and per-table column maps, schemas, and parsed checks/generated expressions within the existing statement arena | Sources release after copying; SQLSTATE 54000 aborts failed capture before mutation; identity-only DELETE avoids unnecessary byte copies |
| Journal and catalog positions | Checked 64-bit positions; object-specific lower OID limits still apply; durable batches need space for their records and commit boundary | Startup-charged controls; SQLSTATE 54000 before wrap, identity reuse or partial catalog publication; [behavior and diagnostics](implemented-baseline.md#numbering-changes-safely) |
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
Enum label images reserve `max_enums + P` shared slots and one replay scratch
image, where `P = min(max_enums * max_catalog_versions_per_object,
(max_connections + max_prepared_transactions + max_subscriptions) *
max_ddl_per_transaction)`. Committed definitions, pending changes and retained
readers share those slots. Reader retention can exhaust this pool before a
pending-definition limit is reached; `54000` names the enum member-image pool
and preserves the published definition. Each image holds
`max_enum_labels_per_type` members, each with a current label, sort position and
optional legacy label binding. Their actual structure sizes, counters and controls are included
in the startup memory plan.
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
- Heap layout, heap-version `ctid`, transaction-header columns (`xmin`, `xmax`,
  `cmin`, `cmax`), HOT, physical vacuum internals, physical
  XLOG/streaming replication, hot standby, and binary-WAL tools are non-goals.
  Object-native row identities are not exposed as invented heap addresses.
- Logical copies are asynchronous independently durable databases; transparent
  shared-storage replicas and active-active writers are not implemented.
- Monitoring fields for absent PostgreSQL subsystems use documented truthful
  empty/zero states; real modeled activity must not be accepted and ignored.
- Smaller definition and XPath subsets remain documented scale/surface
  differences. Compatibility with earlier pos3ql versions is not required;
  unsupported stored formats fail explicitly.
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
