# pos3ql

pos3ql is a database written in Rust that aims to work like PostgreSQL for
applications and people using SQL. Its durable data lives in an S3-compatible
object store. Memory and local disk hold caches that can be rebuilt.

## Where we are

Much of the database works today: SQL, transactions, PostgreSQL clients,
indexes, logical replication, backups, and recovery. Tests compare results,
errors, and client behavior with actual PostgreSQL 18. There are still
[compatibility limits](docs/postgresql-18-compatibility.md); the PostgreSQL
name alone is not a promise that every feature or extension works.

**SQL uses one CPU worker per server process.** Object-store requests can overlap:
independent reads have separate request slots, and transactions can share a
durable log upload while other connections continue. This does not run one SQL
query on several CPU workers. Production performance and failover on separate
machines still need qualification.

For example, a cached invoice can be read while another connection waits for
storage. A successful order commit must survive loss of both local caches.
The [plan](PLAN.md) orders the remaining correctness work, multiple CPU workers
and qualification on separate machines.

See [what works today](docs/implemented-baseline.md) and the
[remaining plan](PLAN.md) for the details and the evidence still needed.

## Where we want to get

A database where you can:

- Use familiar PostgreSQL SQL and client tools, with clear errors for features
  we do not support.
- Run reports and updates at the same time without incorrect results or a
  slow storage request stopping unrelated work.
- Trust a successful commit even if the database machine loses its local disk.
- Replace a failed server, recover from object storage, and route clients to
  the replacement with a tested procedure.
- Choose a memory budget before startup and get an explicit capacity error
  when it is exhausted.
- Judge speed and cost from repeatable measurements against vanilla
  PostgreSQL, including a comparison with the same CPU and memory allowance.

Full saved slices group values by column; small updates use compressed row
groups. Packed objects, ranged reads, immutable indexes and overlapping storage
requests reduce unnecessary work. For example, reading two columns can avoid
fetching a large description column. These are implemented mechanisms; their
production benefit still needs measurement. [Architecture](docs/architecture.md)
explains the request path, optimizations and physical size limits;
[performance](docs/performance.md) distinguishes measured results from estimates.

One active server writes to each configured object-store prefix, the group of
object names belonging to that pos3ql instance and its SQL databases. The goal is concurrent transactions
inside that server. Logical replication creates separate, asynchronously
updated databases. [Replacement](docs/operations.md#controlled-replacement)
transfers write ownership to another server.

## Choose where committed data is saved

| Setting | What happens before a success response | If local disk is lost |
|---|---|---|
| `object_store = off` | The local recovery journal is synced to disk. | Local-only data is lost. |
| `object_store = on` | The transaction data is uploaded and the durable commit record is updated. | The database can recover from object storage. |

In durable mode, transactions can share an upload and commit-record update.
Each success response waits for that work. Checkpoints organize saved data for
recovery and later reads. [Object storage](docs/object-storage.md) and
[durable formats](docs/durable-format.md) explain the failure and upgrade rules.

## Choose how transactions share data

Every configuration must select `transaction_mode = mvcc` or
`transaction_mode = locking`; the server requires `--config`.
Also set `catalog_history_bytes` explicitly: for example, `16MiB` in `mvcc`
mode, or `0` in `locking` mode. This reserves space for older catalog rows such
as table names and column definitions while transactions still need them.

- `mvcc` retains older row values while readers need them. For example, a
  repeatable-read report can keep seeing an order's earlier status while
  another transaction changes it.
- `locking` keeps current committed row values. Conflicting work waits on
  relation locks; work on unrelated tables can continue. This mode supports
  read committed, including PostgreSQL's equivalent read uncommitted mode.
  Repeatable read, serializable and exported snapshots return `0A000`
  because their guarantees require retained versions.

The mode is fixed for the server's lifetime. Uncommitted changes and savepoint
undo remain private in both modes; disabling MVCC does not disable rollback.

## Getting started

Install a checksummed Linux release using the [installation guide](packaging/README.md).
For a development binary built on a suitable host, run:

```sh
pos3ql --config examples/dev.conf
psql -h 127.0.0.1 -p 5433 -U postgres -d postgres
```

In `psql`, try:

```sql
CREATE TABLE orders (id integer PRIMARY KEY, status text);
INSERT INTO orders VALUES (1, 'new');
SELECT status FROM orders WHERE id = 1;
```

This returns `new`. The development configuration saves data locally. To use
object storage, configure a supported endpoint, bucket, unique prefix, TLS,
and a credential file readable only by its owner. Size the startup memory
reservations for the host before starting the server.

## Documentation

The [documentation index](docs/README.md) routes questions about architecture,
SQL limits, scheduling and migrations, backups, storage and performance. [CONTRIBUTING.md](CONTRIBUTING.md)
explains development and checks. [BUGS.md](BUGS.md) lists only known defects
that cannot currently be fixed because of a stated blocker; it is not a list
of all unfinished work. License terms are in [LICENSE](LICENSE).

Copyright (c) Adrian Mârza and pos3ql contributors
