# Copying table changes with logical replication

Logical replication sends table changes to a separate database using
PostgreSQL's publication/subscription protocol. Each database keeps its own
durable state. The subscriber can lag: reading it right after an order update
does not guarantee the update has arrived.

**User example:** on a publisher with an `orders` table, an authorized role
publishes it:

```sql
CREATE PUBLICATION orders_feed FOR TABLE orders;
```

A subscriber needs matching table definitions, connection credentials and a
subscription to `orders_feed`. The initial copy transfers existing rows; later
commits stream as changes. A subscriber is a separate database instance, not
another SQL database inside the same prefix.

pos3ql's recovery journal is not PostgreSQL physical WAL. Physical replication
and logical decoding of prepared transactions are rejected.

## Publisher

- `IDENTIFY_SYSTEM`, logical slot creation, exported snapshots,
  `START_REPLICATION`, keepalives and standby-status feedback use PostgreSQL
  replication-protocol framing.
- pgoutput protocol versions 1–4: text or binary tuples, relation and type
  metadata, replica identities, inserts, updates, deletes, truncation, origins,
  publication column lists and row filters, generated columns, and partition
  routing.
- `messages = true` publishes pgoutput `M` frames from
  `pg_logical_emit_message` (text or binary). Transactional messages keep their
  order among row changes and disappear on rollback; nontransactional messages
  are durable on their own and survive an outer rollback.
- One complete emitted transaction must fit the startup-sized replication
  buffers, and transactional message ordering uses the startup-sized work
  arena. Exhaustion is a named error and never advances the slot.

Relation messages use the same identifier as
`SELECT 'orders'::regclass::oid`; with partition-root publication, the root
table's. A dropped and recreated table gets a new identifier, so it cannot be
mistaken for the old one.

### Keeping the initial copy consistent

**User example:** an order changes while a subscriber copies the initial rows.
The subscriber imports the snapshot returned by `CREATE_REPLICATION_SLOT`, so
its copy sees the earlier order consistently:

```sql
BEGIN ISOLATION LEVEL REPEATABLE READ;
SET TRANSACTION SNAPSHOT '<snapshot_name>';
-- Read or COPY the initial rows here.
COMMIT;
```

Import before sending another command on the exporting replication connection.
Any next command (including `IDENTIFY_SYSTEM` or ordinary SQL), or closing that
connection, expires the export. A successful import keeps its history
protected until its transaction ends.

| Condition | SQLSTATE |
|---|---|
| Import after a query, or inside a savepoint (checked before the name, as in PostgreSQL) | `25001` |
| Malformed name (names are uppercase hexadecimal digits and hyphens) | `22023` |
| Expired, or well-formed but never exported | `42704` |
| Import from another database, or into a serializable transaction | `0A000` |
| Export capacity exhausted (`max_connections` entries reserved at startup, separate from transaction snapshots) | `54000` |

Exhaustion never replaces a valid export or partly imports a snapshot.

## Subscriber

A subscription copies the publisher's initial snapshot with binary COPY, then
applies ordinary and streamed pgoutput transactions. It saves local row changes
and publisher progress together before acknowledging that progress, so after a
crash it never claims a transaction it lost. A delayed worker cannot advance a
replaced subscription, and managed slot cleanup survives crashes.

Workers use their own session settings and an empty `search_path`, as in
PostgreSQL. Schema-qualify tables inside replica-enabled trigger functions, for
example `INSERT INTO public.order_audit ...`.

Target tables:

- Ordinary and partitioned tables are accepted. A view, materialized view,
  foreign table or other relation kind returns `42809`.
- Columns may be in a different order than on the publisher.
- Initial COPY and later changes run row triggers and compute stored generated
  columns using the actual local partition's definition.
- A replicated TRUNCATE of a partitioned table clears its partitions; ordinary
  inheritance children are not included.
- An interrupted apply rolls back the whole local change without advancing its
  acknowledgement.

**User example:** an incoming update moves an order to another local
partition. Replication runs DELETE triggers on the old partition and INSERT
triggers on the new one. An update that stays in its partition runs UPDATE
triggers. SQL UPDATE and MERGE also run BEFORE UPDATE triggers before deciding
whether a move is needed.

Each worker caches publisher table and column names in
`subscription_relation_capacity` entries and rechecks the local table before
applying data, so local schema changes are picked up. A missing published
column, or one that became locally generated, returns `55000` and does not
advance the acknowledgement. A failed update or a full cache keeps earlier
valid entries. The cache is reserved at startup: about 6.5 MiB per worker at the
default 64 entries, or about 104 MiB across the default 16 subscription
workers. Lowering `subscription_relation_capacity` reduces it after restart.
Each worker also reserves one session's cursor pool (`max_cursors` cursors, see
[cursor capacity](postgresql-18-compatibility.md#cursor-capacity)), so a
trigger fired by an applied change can open and fetch SQL cursors; they close
when the applied transaction ends.

The [subscription catalog](https://www.postgresql.org/docs/18/catalog-pg-subscription.html)
is shared across SQL databases. `pg_subscription.subdbid` names each
subscription's owning database; reading the catalog from another database does
not change ownership or start another worker. Subscription names are local to
their database; their object identifiers are unique across the instance.

## SQL and monitoring

- Functions: `pg_create_logical_replication_slot`,
  `pg_copy_logical_replication_slot`, `pg_drop_replication_slot` and
  `pg_replication_slot_advance`. They require a superuser or a role with
  `REPLICATION`.
- Only `pgoutput` logical slots are accepted. Temporary slots and two-phase
  decoding are rejected without side effects; the failover flag is stored as
  slot state.
- Views `pg_replication_slots`, `pg_stat_replication`,
  `pg_stat_replication_slots`, `pg_stat_subscription` and
  `pg_stat_subscription_stats` use PostgreSQL 18 column names and types.
  Durable positions come from the same state used for feedback; activity
  counters are transient and startup-bounded.
- `pg_stat_reset_replication_slot` and `pg_stat_reset_subscription_stats`
  reset those counters and record the reset time.

## Upstream provenance

The protocol and catalog contract was checked on 2026-09-08 against PostgreSQL
18.6 and these sources:

- [Logical replication protocol](https://www.postgresql.org/docs/18/protocol-logical-replication.html)
- [Logical replication message formats](https://www.postgresql.org/docs/18/protocol-logicalrep-message-formats.html)
- [Replication management functions](https://www.postgresql.org/docs/18/functions-admin.html#FUNCTIONS-REPLICATION)
- [Cumulative statistics views and reset functions](https://www.postgresql.org/docs/18/monitoring-stats.html)
- [`pgoutput.c`](https://git.postgresql.org/gitweb/?p=postgresql.git;a=blob;f=src/backend/replication/pgoutput/pgoutput.c;hb=REL_18_STABLE) and [`proto.c`](https://git.postgresql.org/gitweb/?p=postgresql.git;a=blob;f=src/backend/replication/logical/proto.c;hb=REL_18_STABLE)

Snapshot lifetime and import rules were checked on 2026-10-13 against
[the replication protocol](https://www.postgresql.org/docs/18/protocol-replication.html),
[SET TRANSACTION](https://www.postgresql.org/docs/18/sql-set-transaction.html) and
[PostgreSQL's snapshot manager](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/time/snapmgr.c),
and are compared with a real PostgreSQL 18 server in the Drivers workflow.

Partition apply, generated-column timing and accepted target kinds follow the
pinned [replication executor](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/execReplication.c),
[subscription worker](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/replication/logical/worker.c)
and [generated-column rules](https://www.postgresql.org/docs/18/ddl-generated-columns.html).

Tests:

- The external suite uses PostgreSQL 18's own subscriber worker and
  `pg_recvlogical`. One PostgreSQL publisher feeds both PostgreSQL and pos3ql
  subscribers, checking expected rows and trigger events after initial COPY,
  later changes and moves between reordered partitions.
- Raw-wire tests assert exact frames for options, logical-message order and
  binary content, keepalives, feedback and rejection paths.
