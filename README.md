# pos3ql

pos3ql is a PostgreSQL-compatible database engine in Rust. SQL and the PostgreSQL v3 simple and extended protocols are the compatibility boundary. Durable data lives in provider-neutral object storage; RAM and local disk are bounded caches.

## Architecture

- PostgreSQL clients: psql, JDBC, Npgsql, psycopg, node-postgres, and pgx use the ordinary wire protocol, including binary Bind, Result, and COPY for implemented types.
- Durable state: checksummed WAL, immutable SST blocks, and a compare-and-swap manifest. A node can cold-start with an empty local disk.
- Object storage: the engine depends only on immutable or conditional PUT, full/ranged GET, LIST, DELETE, and ETag compare-and-swap. The S3-compatible adapter works with S3 and MinIO; other providers require an adapter or gateway with the same contract.
- Memory: all runtime memory is budgeted at startup. Pools and queues have fixed limits; exhaustion is an error.
- Determinism: the core is event-driven and runs under deterministic fault simulation.

## Durability

| Mode | Acknowledgement | Survives local-disk loss |
|---|---|---|
| `object_store = off` | local WAL sync | no |
| `object_store = on` | local WAL sync and object-store WAL upload | yes |
| VSR (roadmap) | quorum ordering and object-store WAL upload | yes |

With object storage enabled, an acknowledgement waits for durable WAL in the bucket. Checkpoints publish immutable table state through a compare-and-swap manifest; recovery replays newer uploaded WAL. Local disk is never a durable special case in this mode.

## Status

The single-node server supports PostgreSQL v3.0/3.2, TLS, authentication, DDL/DML, transactions and savepoints, row/table locks, views, materialized views, indexes, sequences, domains, enums, SQL functions, CTEs, joins, windows, COPY, logical-replication publishing, and PostgreSQL catalog introspection used by common clients and dump/restore tools.

Verification includes unit/property tests, SQLLogicTest and differential runs against PostgreSQL, psql and driver probes, object-store cold-start and crash recovery, and deterministic storage/consensus simulation.

The completion work is physical-demand propagation through query execution, logical-replication interoperability and subscription, remaining PostgreSQL SQL/catalog/tooling coverage, and production VSR routing. See [PLAN.md](PLAN.md).

## Quick start

```sh
docker run -d -p 19100:9000 -e MINIO_ROOT_USER=minioadmin \
  -e MINIO_ROOT_PASSWORD=minioadmin minio/minio server /data
docker exec <container> mc mb local/pos3ql
cargo run --release -- --config examples/dev.conf
psql -h 127.0.0.1 -p 5433 -U you
```

## Project documents

- [PLAN.md](PLAN.md) — completion roadmap
- [BUGS.md](BUGS.md) — unresolved, genuinely blocked bugs only
- [docs/terminology.md](docs/terminology.md) — naming and glossary
- [AGENTS.md](AGENTS.md) — contribution rules

## References

- [PostgreSQL frontend/backend protocol](https://www.postgresql.org/docs/current/protocol.html)
- [Viewstamped Replication Revisited](https://pmg.csail.mit.edu/papers/vr-revisited.pdf)
- [TigerBeetle safety and design](https://docs.tigerbeetle.com/concepts/safety/)
- [AWS Signature Version 4](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html)
