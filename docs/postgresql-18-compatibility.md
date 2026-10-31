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
| Catalogs/tools | Typed PostgreSQL-shaped catalogs and monitoring for tested psql, pg_dump/pg_restore, psycopg2, psycopg 3, SQLAlchemy, Alembic, pgJDBC, Npgsql, node-postgres, and pgx paths |
| Replication | PostgreSQL publication/subscription and pgoutput interoperability within the [logical replication boundary](logical-replication.md) |
| Persistence | Immutable object-native commits/checkpoints, recovery, backups, and [versioned durable formats](durable-format.md) |

The source repository's `tests/postgresql18_commands.tsv` records executable
commands and explicit architecture rejections. Curated differential queries,
raw-wire/driver probes, and the vendored regression schedule are continuing
ratchets. Unsupported clauses and type combinations must reject explicitly.

Actual application fixtures create and update SQLAlchemy models, inspect their
columns and constraints, stream results, and upgrade and reverse Alembic schema
changes. Both psycopg2 and psycopg 3 run those workflows against PostgreSQL and
pos3ql. For example, an identity column in a non-default schema must still be
discovered correctly when an inspector queries the catalogs. These fixtures
cover the recorded workflows, not every application or library feature.

Array and `CASE` result types come from their declarations, including subqueries
and null values. For example, `ARRAY[(SELECT NULL::date)]` is `date[]`, and
mixing integer and bigint members produces `bigint[]`. An empty constructor
needs an explicit type, such as `ARRAY[]::integer[]`. Type inspection still
evaluates its argument: `pg_typeof(nextval('order_numbers'))` issues one number.
Reached subqueries share the caller's sequence session. Row bounds evaluate
`OFFSET` before `LIMIT`; both can call supported functions or scalar subqueries.
Correlated table functions use enclosing column declarations even for empty or
null inputs. For example, `ARRAY(SELECT d FROM unnest(orders.days) d)` keeps
the declared element type of `orders.days`; a single-column function alias
denotes its scalar value. Result descriptions do not require a sample row.

Arrays of domains over built-in arrays are supported. For example, a domain
`integer_vector` over `integer[]` can be used as `integer_vector[]`.
Arrays of domains whose base is an array of user-defined values are rejected:
the stored base code cannot retain that nested declaration. A scalar domain
containing an array of scalar domains is supported.
For example, `CREATE DOMAIN positive_ids AS positive_integer[]` is distinct
from an unsupported `positive_ids[]` column.

Catalog statistics types `pg_ndistinct`, `pg_dependencies` and `pg_mcv_list`
reject client text and binary input with `0A000`, matching PostgreSQL. Reports
can display their generated values. Copying non-null statistics values into
user tables, including `pg_statistic[]`, is unsupported and fails with `0A000`
before storing unreadable data. NULL values remain valid.

Datetime input uses PostgreSQL 18.6's Default abbreviation definitions and the
current session zone's meanings. For example, `2021-06-01 12:00 EST` means
17:00 UTC even in summer; `America/New_York` follows that date's daylight-time
rule instead. With `TimeZone = 'Asia/Shanghai'`, its `CST` means UTC+8 and takes
precedence over Default's North American UTC−6. Historical abbreviations use
the saved TZif transitions, including in `to_timestamp(..., '... TZ')`.
`pg_timezone_abbrevs` reports these meanings at the transaction's start time.
Named zones require installed files under `/usr/share/zoneinfo`; fixed
abbreviations do not depend on optional legacy files such as `EST`.
The definitions come from [PostgreSQL 18.6's Default set](https://github.com/postgres/postgres/blob/REL_18_6/src/timezone/tznames/Default)
and are checksum-pinned when generating the executable's abbreviation data.
Only the pinned Default set is available. The `timezone_abbreviations` setting
is unsupported and rejected.

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

The [architecture size table](architecture.md#memory-and-physical-size) covers
block, overflow-row, saved-generation, heap and instance-wide inventory limits.
The [performance guide](performance.md#estimating-bandwidth-and-throughput)
explains why none of those numbers establishes a transactions-per-second rate.

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
| PostgreSQL relation shape | 1,600 columns: tables (including retained dropped columns), views, named composites, record definitions, explicit USING lists | `src/storage/rowenc.rs`; wide relation and record differential fixtures |
| PostgreSQL executable result | 1,664 columns: SELECT targets, routine/table-function results, Describe and Bind formats | Wide result and routine fixtures |
| PostgreSQL statement shapes | 100 routine inputs; 32 index/partition attributes; 4,096 grouping sets; 12 CUBE elements; 31 GROUPING arguments | Parser/type boundaries and accepted/rejected differential cases |
| PostgreSQL wire parameters | 65,535 Parse/Bind and SQL PREPARE/EXECUTE parameters | Fixed prepared/portal buffers plus statement memory; maximum-width wire fixture |
| Durable array count | 65,535 elements; exact slices or streaming traversal | Array text/binary, execution, persistence, and cold-recovery fixtures |
| Constraint count | 64 constraints per modeled table kind and 64 domain checks | SQLSTATE 54000 on the next item; changing capacity does not renumber existing rules |
| Durable constraint identity | Creation numbers 1–9,999,999 from the shared catalog counter | CHECK, foreign-key and NOT NULL rules retain identifiers across supported edits and recovery; replacements reserve new numbers; SQLSTATE 54000 on exhaustion |
| Other catalog lifetime identities | Statistics, extensions, foreign-data wrappers/servers/mappings, publications and subscriptions use creation numbers 1–9,999,999 | Shared catalog counter; edits/recovery preserve identity, replacement reserves a new one; `54000` on exhaustion |
| Other definition breadth | Documented 64-item constructs, including LIST bounds, inheritance parents, trigger arguments, and routine configuration entries | Shared parse/storage boundary; widening requires review of representation and identity |
| Stored default expressions | 120 bytes, including enum member bindings and saved catalog display text | Existing startup reservation; SQLSTATE 54000 names an overfull default or bound enum expression before installation |
| Startup capacities | Independent catalog, connection, transaction/savepoint, row-version, lock, replication, cache, and checkpoint pools | Configuration and exact startup memory plan; named errors before partial publication |
| MVCC row history | Explicit shared `row_history_slots` across tables/large objects, plus `max_row_versions_per_row` per row | Updates reserve before durability, including queued/prepared commits; exhaustion is `54000`; locking mode requires zero history slots |
| Durable object requests | `object_store_get_slots` read clients, `object_store_put_slots` journal upload clients, `object_store_block_put_slots` physical upload clients, one separate atomic block client, administrative and independent journal/file cleanup clients, manifest publication clients, and configured credential/ownership clients | Each reserves header and response capacity; journal upload slots also reserve `wal_upload_buffer_bytes` immutable body bytes; physical slots reserve 256 KiB each and share a fixed writer-emission buffer. Read controls, upload outcomes and a 256 KiB block scratch buffer are charged separately from caches; atomic operations may wait synchronously |
| Durable commit completion | `commit_queue_slots` complete transaction and session-context buffers | SQLSTATE 54000 names queue exhaustion before irreversible log staging; locks and outcomes survive storage waits and client disconnects |
| Executing snapshot retention | 1,024 owned statement snapshot and selected row/index file owner slots per query workspace; separate transaction snapshots; checked 64-bit identities; independent `2 * max_value_indexes` retained index graphs | File ownership survives scan/arena release and statement waits; startup membership bits deduplicate aliases without using point-source slots; SQLSTATE 54000 before partial registration, file capture or identity reuse |
| Query SST root retention | 64 source slots per query workspace; configured generation capacity per slot; shared unique-root capacity twice the startup table-slot × generation capacity | Startup-owned roots, schema, and reader buffers; scans with no resident overlay remain usable after table/Storage retirement; SQLSTATE 54000 before partial capture |
| Object-row source retention | 64 point-source slots per query workspace, separate from scan sources; checked shared lease counts | Complete captured generations and newest deletion metadata; SQLSTATE 54000 on exhaustion, without reducing scan capacity |
| Retained row metadata | 64 copied point reads per query workspace; each reserves the configured maximum pending and committed versions | Startup-owned metadata, heap sources, and saved-file roots; SQLSTATE 54000 on exhaustion, with no shared version lock held during retention; captured-table admission returns 40001 before reserving a slot when its identity changed |
| Bound prepared statements | Existing `max_prepared` statement slots and `prepared_bytes` per slot | Releasing a name preserves bytes needed by bound portals; slots are reusable after the last portal closes; exhaustion remains SQLSTATE 54000 |
| Temporary namespaces | One fixed entry per connection/subscription slot; first creation consumes a transaction DDL undo entry | Creation rolls back; an established empty namespace survives ordinary drops and DISCARD, then disappears at disconnect. Namespace state never enters durable records |
| Streamed COPY statement-trigger inputs | `txn_rows` records and `copy_transition_bytes` (256 KiB default) per connection and subscription worker | Startup-owned encoded values; no later source lookup; SQLSTATE 54000 aborts COPY on row/byte exhaustion; cleanup permits a smaller retry |
| Execution schema images | DML output and catalog execution share the startup schema-image pool: table slots × (`max_catalog_versions_per_object` + 1) × `query_workspace_slots`; images release with their owning scope or output analysis | SQLSTATE 54000 before partial description on exhaustion; catalog descriptions retain only their visible/hidden column metadata in the statement arena; full execution schemas use reserved images |
| Resident row metadata | `table_rows` stable row cells per user table, `large_object_pages` for the internal page table; each has a fixed row-identity lookup index with at least eight buckets and at most one-half load | Both arrays and inline controls are charged at startup; a row update at full capacity replaces its cell, while a new row fails with SQLSTATE 54000; lookup backshifts never move another row cell |
| Reader-history cleanup | Fixed cursor, retry and pressure-scan controls; at most 64 stable slot visits per ordinary maintenance turn; no table-sized identity buffer or query walk slot | Normal statement/transaction completion schedules work, including idle/download waits; explicit synchronous completion is available. Every batch rechecks readers; owner contention retries after one second without changing readiness. Later releases cannot be erased by a partial pass |
| Heap location capture | Fixed table/slot cursor in the startup relocation workspace; at most 64 stable slot visits per ordinary turn, with a separate 65,536-unit preparation/copy allowance | Starting does no metadata sweep; busy version/table owners retain the cursor for a healthy one-second retry. Pre-publication errors release private scratch; a published move retains its mapping through errors or task destruction. Late writes and template clones rehome into the append tail |
| Heap address updates | Shared old/new byte owners and a startup-owned mapping; at most 64 stable slots per background preflight or remap turn | Readers retain either allocation; a short heap switch precedes bounded updates of committed, historical and pending locations. No all-table row-map owner in background work |
| Synchronous heap publication | One optional row-map write guard per physical table slot (`max_tables + 1`), with exact aligned arena/mutex controls | Explicit completion may use the synchronous path; partial acquisition releases all earlier owners. SQLSTATE 54000 names an undersized workspace before publication |
| Cache-space waits | At 85% heap use or temporary row-map pressure, eligible writing commands wait before effects while a reclaim task is active | Reads, transaction completion and cancellation proceed. Capacity waits have their own generation and Extension/CacheCapacity event. Pending writes/DDL and an unreclaimable working set retain named capacity errors |
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

Catalog families have separate startup capacities. Set consequential limits
explicitly: for example, omitting `max_extended_statistics` currently derives
its capacity from `max_tables`; the development file states it explicitly. Transaction bounds also cover prepared and subscription
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
the accepted GUC byte representation. Each cursor reserves `cursor_bytes`
for typed rows, `cursor_query_bytes` for its query, copied parameters and their
declared types (a `NULL` date parameter still has the date type),
including local values and types captured by a static procedural `OPEN`,
a 1,024-byte description and `floor(cursor_bytes / 6)` row
positions; the connection reserves the same number of fetch-selection positions.
Each saved row includes a four-byte length and a two-byte column count.
For retained `NO SCROLL` cursors, `cursor_bytes` bounds one fetched batch;
the next fetch reuses it. `MOVE` and rows skipped by forward `ABSOLUTE` or
`RELATIVE` motions do not consume saved-row capacity. A suspended fetch keeps
its already saved rows until it completes. Other cursor paths still save the
whole result during opening.
Long column names can fill the description even when the rows fit. That error
names the description limit: use fewer or shorter labels. Increasing
`cursor_bytes` only changes the row capacity.
Static procedural cursor variables use the shared SELECT and set-query executor.
For example, `OPEN c FOR SELECT label UNION ALL SELECT label` captures `label`
when opened, including its type when the value is null. A variable name that
also names a source column raises `42702`; qualify the table column or rename
the variable. A caller's wire parameter types do not replace a function's own
parameter declarations, including a domain's own identity rather than its base
representation. Dynamic SQL takes parameters only from `USING`: for example,
`OPEN c FOR EXECUTE 'SELECT $1' USING d` captures a null date as a date, even
inside a function whose first argument is an integer. Local names do not become
columns inside the command string. The
[behavior guide](implemented-baseline.md#reads-keep-their-selected-data)
defines which query shapes retain input and execution positions, and when
fetching evaluates expressions or reports their errors.
The retained reader's selected row identities, saved-file
positions and copied saved-row input share `cursor_query_bytes`; exhaustion returns
`54000`. Each query workspace also reserves fixed row and saved-file read
buffers at startup, accounted separately from `work_arena_bytes`.
Resident input retains immutable heap bytes through an owned reader rather
than copying the whole row into that arena. Selected output still must fit
`cursor_bytes`.
The cursor keeps its relation in use through end of input until `CLOSE`.
Destructive table changes in the same session raise `55006`; a table rename
does not change its saved definition or input. This follows PostgreSQL's
[active-relation check](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/tablecmds.c).
Without a scroll option, a retained physical scan is scrollable; a plan with
no input scan is forward-only. `pg_cursors.is_scrollable` reflects that choice.
Constant false or null filters request no rows, including a string literal
coerced to boolean. Type and constant errors precede execution permission checks.
Holdable versions save unread results during commit, preserving the current
position and releasing query/input owners before temporary-table commit actions.
A forward-only held result must fit `cursor_bytes` for its unread suffix;
a scrolling held result must fit its accumulated history and unread suffix.
Deferred constraint triggers execute first. Saving currently runs under the
synchronous commit read scope. `DECLARE ... WITH HOLD` is also accepted without
an explicit transaction, including at extended-protocol `Sync`. Failed commit
or implicit-query rollback closes newly created cursors; cursors held by an
earlier commit survive.
With explicit `SCROLL`, visited results stay in `cursor_bytes` for backward
fetches. A forward jump or `MOVE` also saves newly visited rows; `FETCH LAST`
reads through end of input. This history accumulates across fetches and can
exhaust its named capacity. Forward-only cursors instead reuse their buffer
per fetch. Both modes close their reader on a failed fetch.
With `NO SCROLL`, `MOVE 0` reports whether there is a current row without
reading it. `FETCH 0` before the first row returns no row; on a current row it
raises `55000` because rereading would require a backward step. A fetch that
fails remains failed after rolling back to a savepoint: further fetches raise
`55000` with `portal "name" cannot be run`. Close and reopen the cursor.
`MOVE BACKWARD ALL` before the first row returns zero. A negative absolute
position first consumes the remaining forward input and then fails on its
backward step. Scan statistics begin with the first requested input, including
input consumed by `MOVE` or a forward jump. Read counters in
`pg_stat_xact_user_tables` survive savepoint rollback and release: reading two
rows and rolling back the savepoint still reports two rows read. A replacement
table starts with its own counters, even if it reuses internal storage slots.
JSON/path and XML/XPath limits are specified in [SQL/JSON](sql-json.md) and
[SQL/XML](sql-xml.md). Inspect the memory plan for the chosen configuration;
accepted width can still exhaust its named statement or startup budget.

## Architecture boundaries and remaining gaps

**User example:** an application can receive logically replicated order
changes, but a PostgreSQL physical standby cannot replay pos3ql's journal.
Similarly, familiar index names describe SQL behavior; they do not imply
PostgreSQL's on-disk page layout.

- SQL CPU work uses one worker; eligible object-store reads and uploads overlap.
  [Architecture](architecture.md) separates those paths from the
  [remaining CPU-worker work](../PLAN.md#remaining-work).
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
