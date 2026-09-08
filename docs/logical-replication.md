# Logical replication boundary

pos3ql implements PostgreSQL 18 logical replication where the protocol can be derived from object-native commits. Its local WAL is a checksummed journal, not PostgreSQL physical XLOG; PostgreSQL heap pages, physical replication, and logical decoding of prepared transactions are deliberately rejected.

## Publisher

- `IDENTIFY_SYSTEM`, logical slot creation, exported snapshots, `START_REPLICATION`, keepalives, and standby-status feedback use PostgreSQL replication-protocol framing.
- pgoutput protocol versions 1–4 publish text or binary tuples, relation and type metadata, replica identities, inserts, updates, deletes, truncation, origins, publication projections and row filters, generated columns, and partition routing.
- `messages = true` publishes pgoutput `M` frames. `pg_logical_emit_message` accepts text or binary content. Transactional messages retain SQL command order among row changes and disappear on rollback; nontransactional messages own an independently durable batch and survive an outer rollback.
- One complete emitted transaction must fit the startup-sized replication buffers. Exhaustion is a named error and cannot advance the slot.

## Subscriber

A subscription imports the exported snapshot with binary COPY, then applies ordinary and streamed pgoutput transactions. Its local row changes and publisher frontier commit together before feedback acknowledges the remote LSN. Definition generations prevent a delayed worker from advancing a replaced stream. Managed slot cleanup remains durable work across crashes.

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

The external suite uses PostgreSQL 18's own subscriber worker and `pg_recvlogical`, not a pos3ql-specific stand-in. Raw-wire tests retain exact frame assertions for options, logical-message order and binary content, keepalives, feedback, and rejection paths.
