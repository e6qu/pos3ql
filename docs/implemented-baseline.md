# What works today

This guide summarizes what an application can rely on now. Exact SQL
behavior, error codes and limits are in the
[compatibility guide](postgresql-18-compatibility.md); the
[architecture](architecture.md) explains how the server and object storage
work. Unfinished work lives only in [PLAN.md](../PLAN.md).

## What an application can do

| Task | Current behavior | Detailed guide |
|---|---|---|
| Connect with PostgreSQL clients | SQL, transactions, catalogs, drivers and dump/restore are compared with PostgreSQL 18 | [Compatibility](postgresql-18-compatibility.md) |
| Save an order outside the server | In object-store mode, success waits for durable log publication | [Object storage](object-storage.md) |
| Read selected rows and columns | Immutable indexes and selected-column reads can reduce storage traffic | [Architecture](architecture.md#reads-and-storage-layout), [indexes](index-navigation.md) |
| Recover without local caches | Checkpoint objects plus later committed journal batches rebuild the database | [Recovery](backup-restore.md) |
| Copy selected table changes | Logical subscribers keep separate, asynchronously updated databases | [Replication](logical-replication.md) |
| Operate and replace a server | Readiness, metrics, credential reload, packages and passive-candidate promotion | [Operations](operations.md) |
| Query documents | JSON, JSONB, JSON paths and SQL/XML in their stated forms | [JSON](sql-json.md), [XML](sql-xml.md) |

SQL runs on one CPU worker. Several storage requests can be in flight, and
cached work runs while another query waits for storage. The
[execution model](architecture.md#execution-model) lists the paths that still
block that worker.

## Transactions and durable success

Each server runs in one `transaction_mode`, fixed for its lifetime:

- `mvcc` gives PostgreSQL's snapshots and isolation levels. A
  repeatable-read report keeps seeing an order's earlier status after
  another transaction changes it.
- `locking` keeps only current committed values: read committed (and read
  uncommitted), with conflicting relation operations waiting on locks.
  Repeatable read, serializable and exported snapshots return `0A000`.

Rollback, savepoints and prepared transactions work in both. A commit becomes
visible all at once and only after it is durable; work that completed before a
storage wait (an issued order number, an audit row) is not repeated. Foreign
keys, referential actions and rows changed by concurrent transactions behave
as in PostgreSQL's `READ COMMITTED`, `REPEATABLE READ` and `SERIALIZABLE`. See
[transactions, cursors and application behavior](postgresql-18-compatibility.md#transactions-cursors-and-application-behavior).

When history or lock capacity fills, the statement fails with a named
capacity error (`54000`) instead of growing memory.

## Reads keep their selected data

A query that pauses for storage keeps the rows, definitions and saved files it
selected. Concurrent updates, table drops and maintenance cannot redirect it
to replacement data; a new query sees the replacement.

**Example:** a report pauses after reading one order. Another connection
updates an unread order and maintenance replaces saved files. The resumed
report still returns its original values.

A client that reads slowly never stalls the server. Large results either
stream row by row (waiting for the client and for cold storage without holding
the worker, never repeating delivered rows) or queue in the shared result
spool. A client that falls behind by more than `result_spool_bytes` receives
the rows already queued, then `54000` naming that setting, and its session
stays usable. The [execution model](architecture.md#execution-model) lists
which queries stream.

## Cursors, COPY and routines

- Cursors over tables, constant rows, built-in series and joins of them read
  input only as rows are fetched; other cursors run their query at the first
  `FETCH`, as PostgreSQL does. Row-locking cursors lock each row when a fetch
  reaches it. See [cursors](postgresql-18-compatibility.md#cursors) and
  [cursor capacity](postgresql-18-compatibility.md#cursor-capacity).
- `COPY` follows PostgreSQL's formats, triggers and error order, and a large
  `COPY ... TO STDOUT` streams like a large `SELECT`. See
  [COPY and generated columns](postgresql-18-compatibility.md#copy-and-generated-columns).
- PL/pgSQL and SQL-language routines, triggers, procedures with transaction
  control and `DO` blocks work; a procedure that waits resumes at the waiting
  statement without repeating finished work. See
  [procedural code](postgresql-18-compatibility.md#procedural-code-and-deadlines).
- Volatile functions run where and as often as PostgreSQL runs them, for
  example `nextval` in a sorted, limited query. See
  [when volatile expressions run](postgresql-18-compatibility.md#when-volatile-expressions-run).

## Schema changes and catalogs

Renames keep an object's OID and drop-and-recreate assigns a new one, so a
saved OID never reaches a replacement. A schema change on one table waits only
for that table's locks. Catalog listings follow the query's snapshot while
permission checks use current state, as in PostgreSQL. See
[object identity](postgresql-18-compatibility.md#object-identity-and-bound-references)
and [views, rules and catalog snapshots](postgresql-18-compatibility.md#views-rules-and-catalog-snapshots).

`ANALYZE` produces PostgreSQL's `pg_stats` figures for tables that fit its
sample, and background maintenance never removes data that active readers,
prepared transactions or backups need. See
[statistics and maintenance](postgresql-18-compatibility.md#statistics-and-maintenance)
and [operations](operations.md#scheduled-work-and-schema-migrations).

## What the tests establish

Allocation guards, fault simulation, recovery after deleting local data, and
comparisons with real PostgreSQL and real drivers test these boundaries;
[Contributing](../CONTRIBUTING.md#verification) lists the gates. They do not
establish production performance, complete PostgreSQL compatibility or
failover latency; see the [performance guide](performance.md). Earlier
implementation narratives are in the [history index](history/README.md).
