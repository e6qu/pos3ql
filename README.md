# pos3ql

pos3ql is a PostgreSQL-compatible database engine in Rust. SQL and the PostgreSQL v3 simple and extended protocols are the compatibility boundary. Durable data lives in provider-neutral object storage; RAM and local disk are bounded caches.

## Architecture

- PostgreSQL clients: psql, JDBC, Npgsql, psycopg, node-postgres, and pgx use the ordinary wire protocol, including catalog-typed text/binary Bind, Result, COPY, and set-returning integer/numeric/temporal output for implemented types.
- Durable state: immutable commit batches, immutable SST blocks, and compare-and-swap roots. A node can cold-start with an empty local disk.
- Object storage: the engine depends only on a generic gateway with immutable or conditional PUT, full/ranged GET, LIST, DELETE, and strong-ETag compare-and-swap. Provider protocols and SDKs are outside the application. [Contract and qualification](docs/object-storage.md).
- Memory: all runtime memory is budgeted at startup. Pools and queues have fixed limits; exhaustion is an error.
- Determinism: the core is event-driven and runs under deterministic fault simulation.

## Durability

| Mode | Acknowledgement | Survives local-disk loss |
|---|---|---|
| `object_store = off` | local journal sync | no |
| `object_store = on` | immutable commit batch PUT and commit-head CAS | yes |

With object storage enabled, the server groups transactions received in one readable protocol batch, publishes their immutable journal bytes, then advances a CAS commit head before releasing success responses. Checkpoints publish immutable table state through a separate CAS manifest. Recovery follows the commit head beyond that manifest; local disk is a cache.

## Status

The single-node server supports PostgreSQL v3.0/3.2, TLS, authentication, DDL/DML, transactions and savepoints, row/table locks, views, materialized views, indexes, sequences, domains, enums, PostgreSQL large objects, full-text search, SQL functions (scalar, `SETOF`, and `TABLE`, including mutable and nested calls), CTEs, joins, windows, COPY, PostgreSQL 18 SQL/JSON and SQL/XML, PostgreSQL 18-interoperable logical-replication publishing and bounded subscription bootstrap/apply, and PostgreSQL catalog introspection used by common clients and dump/restore tools.

SQL/JSON includes first-class `jsonpath`/`jsonpath[]`, strict and lax path execution, path operators and functions, SQL-standard query and construction functions, `JSON_TABLE`, record conversion, SQL/JSON aggregates, and JSONB read/write subscripting. These types and expressions cross text/binary wire, COPY, stored-query, PL/pgSQL, WAL, checkpoint, and object-cold recovery boundaries. [SQL/JSON compatibility and limits](docs/sql-json.md).

SQL/XML includes first-class `xml`/`xml[]`, constructors and predicates,
bounded namespace-aware XPath, ordered `XMLAGG`, typed `XMLTABLE`, and the
session-visible `xmloption` input mode. The same wire, COPY, stored-query,
PL/pgSQL, WAL, checkpoint, and object-cold recovery boundaries apply.
[SQL/XML compatibility and limits](docs/sql-xml.md).

Permanent, unlogged, and session-temporary tables, indexes, identity sequences, standalone sequences, CTAS, and `SELECT INTO` have distinct PostgreSQL lifetimes. Temporary relations use isolated per-connection namespaces and `ON COMMIT` actions and never enter WAL, checkpoints, object storage, or logical publications. Committed temporary rows spill to a bounded, startup-sized local store (`temporary_spill_bytes`, or `0` to keep them resident-only) that is recreated empty on restart. Unlogged definitions are durable, retain rows after a clean shutdown, and reset table and sequence state after an unclean restart.

Verification includes unit/property tests, SQLLogicTest and differential runs against PostgreSQL, psql and driver probes, object-store cold-start and crash recovery, and deterministic storage fault simulation.

All 183 PostgreSQL 18 top-level commands have a tested execution contract or an explicit architecture boundary; `tests/postgresql18_commands.tsv` is the ratchet. Logical replication interoperates with PostgreSQL 18 publishers, subscribers, and `pg_recvlogical` at the object-native boundary, including transactional and nontransactional logical messages, typed slot-management SQL, and replication monitoring views. Continuing differential, driver, and dump/restore testing remains the compatibility-discovery ratchet. Physical demand is proven through query execution and DML sources; PostgreSQL physical/binary-WAL replication is not a target. See [PLAN.md](PLAN.md) and [the logical-replication boundary](docs/logical-replication.md).

## Quick start

```sh
# Start any implementation of docs/object-storage.md's gateway contract.
cargo run --release -- --config examples/dev.conf
psql -h 127.0.0.1 -p 5433 -U you
```

## Project documents

- [PLAN.md](PLAN.md) — completion roadmap
- [BUGS.md](BUGS.md) — unresolved, genuinely blocked bugs only
- [docs/terminology.md](docs/terminology.md) — naming and glossary
- [docs/object-storage.md](docs/object-storage.md) — portable durability contract
- [docs/logical-replication.md](docs/logical-replication.md) — PostgreSQL 18 protocol, SQL, monitoring, and architecture boundary
- [docs/sql-json.md](docs/sql-json.md) — PostgreSQL 18 SQL/JSON, jsonpath, wire, and durability boundary
- [docs/sql-xml.md](docs/sql-xml.md) — PostgreSQL 18 SQL/XML, XPath, wire, and durability boundary
- [AGENTS.md](AGENTS.md) — contribution rules

## References

- [PostgreSQL frontend/backend protocol](https://www.postgresql.org/docs/current/protocol.html)
- [TigerBeetle safety and design](https://docs.tigerbeetle.com/concepts/safety/)
