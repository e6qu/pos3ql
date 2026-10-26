# What works today

Use this guide for application behavior. The [architecture](architecture.md)
explains how the server and object storage work; [compatibility](postgresql-18-compatibility.md)
owns exact SQL and capacity limits. [PLAN.md](../PLAN.md) contains unfinished
work, rather than a history of completed changes.

## What an application can do

| Task | Current behavior | Detailed guide |
|---|---|---|
| Connect with PostgreSQL clients | SQL, transactions, catalogs, drivers and dump/restore are compared with PostgreSQL 18 | [Compatibility](postgresql-18-compatibility.md) |
| Save an order outside the server | In object-store mode, success waits for durable log publication | [Object storage](object-storage.md) |
| Read selected rows and columns | Immutable indexes and selected-column reads can reduce storage traffic | [Architecture](architecture.md#reads-and-storage-layout), [indexes](index-navigation.md) |
| Recover without local caches | Checkpoint objects plus later committed journal batches rebuild the database | [Recovery](backup-restore.md) |
| Copy selected table changes | Logical subscribers keep separate, asynchronously updated databases | [Replication](logical-replication.md) |
| Operate and replace a server | Readiness, metrics, credential reload, packages and passive-candidate promotion are implemented | [Operations](operations.md) |
| Query documents | JSON, JSONB, JSON paths and SQL/XML have stated supported forms and limits | [JSON](sql-json.md), [XML](sql-xml.md) |

SQL uses one CPU worker. Several storage requests can be in flight, and cached
work can run while an eligible query waits for storage. Multiple CPU workers,
complete shared catalog publication and representative deployment measurements
remain in the plan.

## Transactions and durable success

Every configuration explicitly selects `transaction_mode` and
`catalog_history_bytes` before startup:

| Mode | Reader and writer behavior | Isolation |
|---|---|---|
| `mvcc` | Keeps earlier values while readers need them | Supported PostgreSQL snapshot/isolation behavior, subject to fixed history capacity |
| `locking` | Keeps current committed values; conflicting relation operations wait | Read committed, including PostgreSQL's equivalent read uncommitted; repeatable read, serializable and exported snapshots return `0A000` |

Use a positive catalog-history reservation in MVCC mode and zero in locking
mode. Rollback, savepoints and private uncommitted changes work in both modes.
A long report can retain row and catalog history until their fixed pools fill;
the server returns a named capacity error instead of growing memory.

**Example:** in MVCC mode, a repeatable-read report can continue seeing an
order's earlier status after another transaction changes it. In locking mode,
a conflicting writer waits for the reader's relation lock. Work against an
unrelated table can continue during that wait.

A commit reserves journal positions, uploads its durable data, then installs
its complete transaction before releasing locks and success responses. New
statements cannot select a half-installed order-and-invoice update. Existing
readers retain their selected data. Prepared commit and rollback decisions
retain both the transaction and prepared-slot ownership through completion.

A lost final reply leaves the client uncertain whether the commit happened.
Check application state before retrying an operation that could charge twice.
If installation fails after durability, the server rejects further work with
`08007` and requires restart and recovery; it does not undo durable changes.

## Account identity and permissions

A connection follows the account that logged in, including through renames:

```sql
ALTER ROLE orders_app RENAME TO orders_writer;
CREATE ROLE orders_app LOGIN;
```

The existing connection becomes `orders_writer`. The new `orders_app` does not
inherit that connection, its permissions or its cancellation rights. If the
original account is deleted, simple constant queries remain possible, but
`current_user` and `session_user` return `42704`; object creation returns `42501`.
Renaming a prepared transaction's owner changes the displayed name; recreating
a deleted owner's name does not gain the right to resolve that transaction.
Superusers can still resolve it.

Deleting a role waits for unfinished ownership or permission changes and checks
dependencies across SQL databases. Savepoint rollback releases locks acquired
after that savepoint. Prepared transactions retain their locks through recovery;
`pg_locks` uses durable account and relation identities.

Permission changes on one object wait for its transaction, while changes on
other objects can proceed. Permission lists preserve grant order and explicitly
empty lists through restart. Revoking the owner's own SELECT permission does
not remove its right to grant SELECT.

Numeric missing-object permission checks follow PostgreSQL's account-dependent
rules. A missing table, sequence or large-object identifier returns NULL. For a
missing type, schema, function, language, database, tablespace, foreign-data
wrapper or foreign server, a superuser receives true and an ordinary account
receives NULL. Missing names and invalid privilege names still raise errors.

## Reads keep their selected data

A read retains its read position, selected definitions, original resident bytes
and saved files across suspension. Removing a table, recycling an internal slot,
replacing a file list or moving the row heap cannot redirect that selection to
replacement data. Downloads and callbacks run after short metadata guards end.
Scans capture resident row choices once; saved rows continue streaming.

**Example:** a report pauses after reading one order. Another connection updates
an unread order and maintenance replaces saved files. Resuming the report still
uses its captured values and files. A new read can select the replacement.
These storage protections do not yet establish every later catalog lookup's
coherence; the remaining cases are explicit in the plan.

Statements, transaction snapshots, scans and point reads have separate startup
reservations. Their [exact capacities](postgresql-18-compatibility.md#capacity-boundaries)
are independent of total durable row count. Completion, cancellation, errors
and rollback release retained owners. Cleanup respects active readers and
backup roots. Retaining both row heaps can temporarily prevent reclamation and
return `54000`.

UPDATE, DELETE, MERGE, conflict handling and schema validation preserve the
original selected row values in fixed statement memory. They recheck the table
lifetime and definition before installing a write; a changed source returns
`40001`. A full rollback log rejects and removes only the change it could not
record. Metadata-only ALTER avoids rewriting rows when their representation is
unchanged.

A physical table rewrite follows PostgreSQL's [MVCC caveat](https://www.postgresql.org/docs/18/mvcc-caveats.html):
an older repeatable-read transaction first opening that table after the rewrite
can see it as empty. Existing relation locks and already retained byte sources
have their own protections; a rewrite is not a promise to retain every old
logical table image indefinitely.

## COPY, functions and retries

`COPY FROM` retains its command and read position across input messages.
Statement triggers receive inserted values saved after BEFORE triggers and
generated-column calculation, before AFTER row triggers. An AFTER row trigger
cannot rewrite those saved transition values. Each connection and subscription
worker has `copy_transition_bytes` plus `txn_rows` records reserved at startup.
Exhaustion returns `54000` and aborts COPY; cleanup permits a smaller retry.

`COPY TO` retains the selected schema and row sources and releases them on
completion or error. Table form reads only that relation; use query form to
include partitions, views or an explicit order:

```sql
COPY (SELECT id, status FROM orders ORDER BY id) TO STDOUT (FORMAT csv);
```

A partitioned parent without the applicable row-security rewrite returns
`42809`. Direct view and foreign-table exports require query form. Unpopulated
materialized views return `0A000` to direct COPY. Direct catalog-table COPY is
an explicit limit; use `COPY (SELECT ...)`.

State-changing functions and commands use a separate synchronous block client
when restarting their call stack could repeat effects. They can still block
the SQL CPU worker on an uncached read. Eligible pure reads retain completed
columns and navigation across asynchronous waits. Sequence calls retain their
completed results through command retries; memory exhaustion returns `54000`,
`volatile sequence replay exceeds statement memory`, before an unrecorded
sequence change. Nested SQL keeps the original statement deadline.

Ordinary `nextval` is not rolled back; a transactional restart is:

```sql
CREATE SEQUENCE order_numbers START 10;
SELECT nextval('order_numbers'); -- 10
BEGIN;
ALTER SEQUENCE order_numbers RESTART WITH 50;
SELECT nextval('order_numbers'); -- 50
ROLLBACK;
SELECT nextval('order_numbers'); -- 11
```

Deferred checks run before implicit and explicit commits, including COPY and
extended-protocol Sync. A failed deferred check rolls back the command rather
than confirming a partial import. Volatile casts execute at runtime; skipped
branches do not consume sequence values. Truly immutable constant expressions
can raise planning errors even when a false filter removes every output row,
following PostgreSQL's [volatility rules](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/include/catalog/pg_proc.dat).

## Schema changes and object identity

Schema, table and type edits retain their original object and revision through
waits and rollback. They update bound columns, indexes, comments, foreign keys,
owned sequences and saved queries together within their implemented publication
boundary. A stale edit cannot modify a replacement in the same slot. Waiting
releases earlier owners rather than holding unrelated tables.

Renames preserve object identity; drop and recreate reserve fresh identities
for relations, row/array types, named types, enum members, indexes, CHECK,
foreign-key and NOT NULL constraints, statistics, extensions, foreign-data
wrappers, servers, user mappings, publications and subscriptions. Removing an
earlier column or rule does not renumber survivors. Foreign-key triggers retain
their constraint links. `ADD CONSTRAINT ... USING INDEX` preserves the existing
index identifier. Remaining identifier families are tracked in the plan.

```sql
CREATE TABLE orders(id integer);
CREATE TABLE saved_order_identity AS SELECT 'orders'::regclass::oid AS identity;
DROP TABLE orders;
CREATE TABLE orders(id integer);
SELECT has_table_privilege(identity, 'SELECT') IS NULL FROM saved_order_identity;
-- true: the old identifier does not grant access to the replacement
```

Saved type references and enum constants follow their original type/member,
including arrays, records and defaults. Renaming enum label `pending` to
`waiting` changes its display without rewriting table rows. Creating a new
label named `pending` cannot redirect a previously bound constant. Plain text
is unchanged. Domain `pg_type.typdefault` can preserve the original cached text;
`pg_get_expr(typdefaultbin, 0)` renders its current bound expression.

A table or view reserves its row-type name in the schema. A named type cannot
use the same name. Schema moves keep bound references; string SQL bodies and
explicit text-based name lookups keep their text and can fail after a rename.
`search_path` skips schemas without USAGE. Numeric sequence identifiers bypass
schema-name lookup but still require sequence permissions.

Dropped attributes keep their PostgreSQL column numbers and physical type
metadata. Adding a column appends a new number; the 1,600-column limit includes
retained dropped entries. NOT NULL constraint names survive table and column
renames. `UNIQUE NULLS NOT DISTINCT` treats two NULL-bearing keys as conflicting;
`NULLS DISTINCT` permits them.

Validation checks current committed rows and earlier own changes without
replacing the caller's ordinary query snapshot. Unique index creation cannot
miss a duplicate committed after an older report started. Domain validation
uses compatible table locks and rejects unsupported validation inside stored
arrays/composites with `0A000`. Adding a rule NOT VALID can defer existing-row
validation where supported. PostgreSQL's [table validation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/tablecmds.c),
[domain validation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/typecmds.c)
and [index build](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/access/heap/heapam_handler.c)
provide the reference behavior.

## Views, saved results and rules

`CREATE OR REPLACE VIEW` preserves the view, row type, array type, `_RETURN` rule,
owner, permissions and comments. Its pending definition still needs reserved
memory, even when no additional catalog slot is needed. A source-column rename
preserves the view's output column names.

A materialized view stores results. `WITH NO DATA` makes ordinary reads fail
with `55000`, including LIMIT 0; an optimized constant-false query can avoid
the scan. REFRESH requires MAINTAIN, runs as the owner and retains its exclusive
relation lock until transaction end. Rollback restores both rows and population
status. Prepared decisions, WAL and empty-cache checkpoint recovery retain
those states and identities.

Rewrite rules take the target relation's exclusive lock. A rule that redirects
an insert therefore conflicts with an ordinary read of that target, while
unrelated tables remain usable. Replacement keeps enable mode and comments;
`_RETURN` cannot be renamed. View rule/trigger mode changes and materialized-view
mode changes retain PostgreSQL's explicit relation-kind errors.

## Catalog listings and current lookups

MVCC catalog listings use their statement or transaction's selected rows.
Read committed can select newer rows; repeatable read retains its earlier
listing. Supported PostgreSQL and information-schema reports join those base
rows, with PostgreSQL's current permission and role-membership checks.

**Example:** another connection renames `orders` to `sales`. An older report
can still list `orders`, while `to_regclass('sales')` resolves the current name.
Likewise, an older `pg_get_viewdef` can show a prior query while executing the
view uses the current rule. Dropping the underlying relation can make the old
rule fail to deparse with `XX000`; looking up its missing current identifier
can return NULL. Freezing every lookup at the report's read position would be
incorrect. The reference is PostgreSQL's [rule lookup](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/ruleutils.c).

Permission listings retain selected grants but print current role names.
`pg_get_acl` uses current permissions. Information-schema reports can hide a
definition when the user can execute/read the object but does not own it.
Account reports mask or restrict password fields. View dependencies deduplicate
repeated references and require the applicable ownership role. Their full
result shapes and definitions follow the pinned PostgreSQL 18.6
[information schema](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/information_schema.sql).

Catalog descriptions retain only their actual column metadata in statement
memory; full execution definitions use the existing fixed schema-image pool.
Joining several small catalogs does not reserve a full wide table and all its
constraint arrays for each description. Pool exhaustion is still a named error.

Catalog type properties describe PostgreSQL's interface, independently of the
object-store layout. Hidden `tableoid` must be named explicitly. Index vectors
start at subscript zero and preserve that bound when cast to ordinary arrays;
the reverse SQL casts are rejected with `42846`. `pg_node_tree` values can be
copied and deparsed, but raw client text/binary input returns `0A000`. A catalog
relation can resolve by name even when querying its raw rows is unsupported;
that read must return `0A000`.

## Statistics and maintenance

ANALYZE uses current committed rows and earlier own changes. PostgreSQL gives
relation counts and column estimates different rollback behavior: `reltuples`
can remain changed while column estimates revert. Journal capture and checkpoint
completion clear only the exact statistics revision they saved; later analysis
remains pending. Table replacement rejects a stale capture with `40001`.

`pg_index` lists actual visible indexes. `pg_class.relhasindex` is a hint and
can stay true after DROP INDEX or rollback; VACUUM clears it when appropriate.
Index creation can refresh the row estimate. These values and prepared ANALYZE
state survive recovery with their respective transaction rules.

Automatic maintenance advances between client messages and while idle, including
while eligible downloads wait. Row-history cleanup, eviction and address updates
visit at most 64 stable row slots per ordinary turn. Heap preparation/copying
has a separate work allowance; a slot quota is not a wall-time guarantee.
Owner contention retries on the existing one-second cadence. Completion
preserves newer writes, active readers, prepared changes and backup roots.
Explicit CHECKPOINT, shutdown and some atomic reads still have synchronous work.

At 85% heap use, eligible top-level writes can wait before effects when an active
maintenance task can free space. They preserve their command, timestamp and
deadline and appear as Extension/CacheCapacity waits. Reads and transaction
completion remain runnable. A transaction does not wait for space that its own
pending changes pin. When work cannot fit, it receives its named capacity error.
See [operations](operations.md#scheduled-work-and-schema-migrations) for what
runs automatically and what an operator must schedule.

### Numbering changes safely

Journal positions and internal change counters use checked arithmetic; their
64-bit representation does not guarantee unlimited SQL identifiers. Exhausted
journal positions return `54000`, `WAL LSN space exhausted`. Catalog reservation
errors name the exhausted object's OID range. Existing objects and saved
identities remain readable; new reservations cannot wrap or reuse old values.
The [durable format guide](durable-format.md) owns exact encodings and ranges.

## What the tests establish

Allocation guards, fault simulation, recovery after local-data deletion, real
PostgreSQL comparisons and actual drivers test the implemented boundaries.
[Contributing](../CONTRIBUTING.md#verification) defines the complete PR gates.
Passing those gates does not establish production performance, complete
PostgreSQL compatibility or deployment failover latency.

The [performance guide](performance.md) distinguishes retained measurements,
calculated examples and outstanding qualification. The [history index](history/README.md)
preserves earlier implementation narratives and their evidence.
