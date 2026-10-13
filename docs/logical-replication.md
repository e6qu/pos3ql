# Copying table changes with logical replication

Logical replication sends table changes to a separate database using
PostgreSQL's publication/subscription protocol. Each database saves its own
data. The subscriber can lag behind the publisher; reading it immediately after
an order update does not guarantee that the update has arrived.

**User example:** on a publisher with an existing `orders` table, an authorized
role can select that table for replication:

```sql
CREATE PUBLICATION orders_feed FOR TABLE orders;
```

A subscriber needs matching table definitions, connection credentials, and a
subscription to `orders_feed`. Bootstrap copies the initial rows; later commits
send changes. The publisher and subscriber paths are tested with real PostgreSQL.
This is not a physical PostgreSQL standby or a second writer sharing one prefix.

pos3ql's recovery journal is not PostgreSQL physical XLOG. Physical replication
and logical decoding of prepared transactions are explicitly rejected.

## Publisher

- `IDENTIFY_SYSTEM`, logical slot creation, exported snapshots, `START_REPLICATION`, keepalives, and standby-status feedback use PostgreSQL replication-protocol framing.
- pgoutput protocol versions 1–4 publish text or binary tuples, relation and type metadata, replica identities, inserts, updates, deletes, truncation, origins, publication projections and row filters, generated columns, and partition routing.
- `messages = true` publishes pgoutput `M` frames. `pg_logical_emit_message` accepts text or binary content. Transactional messages retain SQL command order among row changes and disappear on rollback; nontransactional messages own an independently durable batch and survive an outer rollback.
- Transactional message ordering uses the startup-sized work arena; there is no separate message-count ceiling. Work-memory exhaustion is a named error.
- One complete emitted transaction must fit the startup-sized replication buffers. Exhaustion is a named error and cannot advance the slot.

### Keeping the initial copy consistent

**User example:** an order changes while a subscriber copies the publisher's
initial rows. The subscriber imports the snapshot returned by
`CREATE_REPLICATION_SLOT`, so its copy reads the earlier order consistently:

```sql
BEGIN ISOLATION LEVEL REPEATABLE READ;
SET TRANSACTION SNAPSHOT '<snapshot_name>';
-- Read or COPY the initial rows here.
COMMIT;
```

Import before sending another command on the exporting replication connection.
Any next command, including `IDENTIFY_SYSTEM` or ordinary SQL, expires the export;
closing that connection also expires it. A successful import keeps its own
history protection until its transaction ends. An expired identifier returns
`22023`; importing from another database or into a serializable transaction
returns `0A000`, matching PostgreSQL's replication snapshot rules.

Validity and history protection are updated under one short guard. Replacement
cannot briefly release retained history; import checks capacity before changing
the importing transaction. Exports reserve `max_connections` entries at startup,
separately from transaction snapshot entries. Exhaustion returns `54000` without
replacing a valid export or partly importing a snapshot. These protections do
not enable parallel SQL execution.

## Subscriber

A subscription copies the publisher's initial snapshot with binary COPY, then
applies ordinary and streamed pgoutput transactions. It saves local row changes
and its publisher progress together before acknowledging that progress. If it
crashes and restarts, it must not claim to have saved a transaction it lost.
Definition generations prevent a delayed worker from advancing a replaced
stream. Managed slot cleanup remains durable work across crashes.

## SQL and monitoring

The SQL control surface includes `pg_create_logical_replication_slot`, `pg_copy_logical_replication_slot`, `pg_drop_replication_slot`, and `pg_replication_slot_advance`. Only `pgoutput` logical slots are accepted. Temporary slots and two-phase decoding reject atomically; failover is retained as typed slot state. Slot operations require a superuser or a role with `REPLICATION`.

`pg_replication_slots`, `pg_stat_replication`, `pg_stat_replication_slots`, `pg_stat_subscription`, and `pg_stat_subscription_stats` expose PostgreSQL 18 column names and declared types. Durable positions come from the same slot and subscription state used for feedback. Activity and cumulative counters are startup-bounded transient state; `pg_stat_reset_replication_slot` and `pg_stat_reset_subscription_stats` reset those counters and record the reset time.

## Upstream provenance

The protocol and catalog contract was checked on 2026-09-08 against PostgreSQL 18.6 and these upstream PostgreSQL 18 sources:

- [Logical replication protocol](https://www.postgresql.org/docs/18/protocol-logical-replication.html)
- [Logical replication message formats](https://www.postgresql.org/docs/18/protocol-logicalrep-message-formats.html)
- [Replication management functions](https://www.postgresql.org/docs/18/functions-admin.html#FUNCTIONS-REPLICATION)
- [Cumulative statistics views and reset functions](https://www.postgresql.org/docs/18/monitoring-stats.html)
- [`pgoutput.c`](https://git.postgresql.org/gitweb/?p=postgresql.git;a=blob;f=src/backend/replication/pgoutput/pgoutput.c;hb=REL_18_STABLE) and [`proto.c`](https://git.postgresql.org/gitweb/?p=postgresql.git;a=blob;f=src/backend/replication/logical/proto.c;hb=REL_18_STABLE)

Snapshot lifetime, row visibility, database rejection, and serializable rejection
are also compared with an actual PostgreSQL 18 server in the Drivers workflow.
The upstream lifetime and import restrictions were checked on 2026-10-13 against
[the replication protocol](https://www.postgresql.org/docs/18/protocol-replication.html),
[SET TRANSACTION](https://www.postgresql.org/docs/18/sql-set-transaction.html), and
[PostgreSQL's snapshot manager](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/time/snapmgr.c).

The external suite uses PostgreSQL 18's own subscriber worker and `pg_recvlogical`, not a pos3ql-specific stand-in. Raw-wire tests retain exact frame assertions for options, logical-message order and binary content, keepalives, feedback, and rejection paths.
