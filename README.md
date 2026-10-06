# pos3ql

pos3ql is a PostgreSQL-compatible database engine in Rust. SQL, catalogs, and
the PostgreSQL v3 wire protocol define its client boundary. In durable mode,
object storage holds the authoritative data; RAM and local disk are bounded,
disposable caches.

## Status

The engine implements broad PostgreSQL 18 SQL, types, procedural execution,
client tooling, and logical replication. Compatibility is verified against
actual PostgreSQL; unsupported behavior must fail explicitly. The
[compatibility matrix](docs/postgresql-18-compatibility.md) records accepted
forms, capacity limits, and architecture boundaries.

Query execution still runs serially in one process. Independent statement
workspaces and synchronized catalogs prepare for fixed workers, but do not yet
provide concurrent execution. Representative production performance remains
unqualified. [PLAN.md](PLAN.md) records the remaining work and completion gates.

## Architecture

- **Durability:** immutable commit batches and SST blocks, published through
  compare-and-swap roots. Recovery can start with both local caches empty.
- **Ownership:** one writer incarnation per object prefix. Promotion fences the
  previous process before the replacement becomes active.
- **Object storage:** one direct S3-compatible client for qualified endpoints,
  with conditional PUT, full/ranged GET, paginated LIST, DELETE, signing, and
  opaque entity tags. No provider-specific database behavior.
- **Memory:** pools, queues, and execution buffers are charged at startup.
  Exhaustion is a named error. Object-store TLS has an isolated allocation budget.
- **Execution:** event-driven dispatch, MVCC, bounded locks and retries, and
  deterministic fault simulation. Object-native btree, hash, BRIN, GiST, GIN,
  and SP-GiST paths retain SQL and MVCC rechecks.
- **Replication:** PostgreSQL logical publications and subscriptions create
  independently durable copies. Physical PostgreSQL XLOG and shared-storage
  replicas are outside the implemented boundary.

## Durability modes

| Configuration | Success acknowledgement | Survives local-disk loss |
|---|---|---|
| `object_store = off` | Local journal sync | No |
| `object_store = on` | Immutable commit-batch PUT and commit-head CAS | Yes |

In durable mode, transactions completed in one reactor turn share publication.
Success responses wait for the durable barrier. Checkpoints publish a separate
CAS manifest; recovery replays the commit head beyond that manifest. See
[object storage](docs/object-storage.md) and [durable formats](docs/durable-format.md).

## Getting started

Install a checksummed Linux release using the [installation guide](packaging/README.md).
For a development binary built on a suitable host, run:

```sh
pos3ql --config examples/dev.conf
psql -h 127.0.0.1 -p 5433 -U postgres -d postgres
```

The development configuration uses local-only durability. Before enabling
object storage, configure a qualified endpoint, bucket, unique prefix, TLS,
and an owner-only credential file. All startup capacities must fit the host.
Build and validation guidance is in [CONTRIBUTING.md](CONTRIBUTING.md).

## Documentation

| Task | Document |
|---|---|
| Understand current capabilities | [Implemented baseline](docs/implemented-baseline.md), [compatibility matrix](docs/postgresql-18-compatibility.md) |
| Choose the next engineering task | [Roadmap and gaps](PLAN.md) |
| Configure and operate a server | [Operations](docs/operations.md), [installation](packaging/README.md) |
| Back up, export, or recover | [Backup and restore](docs/backup-restore.md) |
| Understand persistence and indexes | [Object storage](docs/object-storage.md), [durable formats](docs/durable-format.md), [index navigation](docs/index-navigation.md) |
| Measure against vanilla PostgreSQL | [Performance](docs/performance.md), [retained benchmark evidence](benchmarks/README.md) |
| Contribute | [CONTRIBUTING.md](CONTRIBUTING.md), [AGENTS.md](AGENTS.md), [terminology](docs/terminology.md) |

The [documentation index](docs/README.md) includes specialized SQL boundaries,
fixture provenance, and historical records. [BUGS.md](BUGS.md) holds only
externally blocked or genuinely intractable defects. License terms are in
[LICENSE](LICENSE).

Copyright (c) Adrian Mârza and pos3ql contributors
