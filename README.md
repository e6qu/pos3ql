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

The single-node server supports PostgreSQL v3.0/3.2, TLS, authentication, DDL/DML, transactions and savepoints, row/table locks, views, materialized views, indexes, sequences, domains, enums, SQL functions (scalar, `SETOF`, and `TABLE`, including mutable and nested calls), CTEs, joins, windows, COPY, logical-replication publishing and bounded subscription bootstrap/apply, and PostgreSQL catalog introspection used by common clients and dump/restore tools.

Verification includes unit/property tests, SQLLogicTest and differential runs against PostgreSQL, psql and driver probes, object-store cold-start and crash recovery, and deterministic storage fault simulation.

The completion work is object-native logical-replication interoperability where practical and remaining PostgreSQL SQL/catalog/tooling coverage. Physical demand is proven through query execution and DML sources; PostgreSQL physical/binary-WAL replication is not a target. See [PLAN.md](PLAN.md).

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
- [AGENTS.md](AGENTS.md) — contribution rules

## References

- [PostgreSQL frontend/backend protocol](https://www.postgresql.org/docs/current/protocol.html)
- [TigerBeetle safety and design](https://docs.tigerbeetle.com/concepts/safety/)
