# pos3ql

pos3ql is a database written in Rust that works like PostgreSQL 18 for
applications and SQL users. Its durable data lives in an S3-compatible object
store; memory and local disk are caches that can be rebuilt.

## Where we are

SQL, transactions, PostgreSQL clients and drivers, indexes, logical
replication, backups and recovery from object storage work today. CI compares
results, errors and client behavior with PostgreSQL 18. Unsupported features
return explicit errors; the [compatibility guide](docs/postgresql-18-compatibility.md)
lists the limits.

SQL runs on one CPU worker per server. Object-store requests overlap, and
transactions can share one durable upload. For example, a cached invoice can be
read while another connection waits for storage. Performance and failover on
separate machines have not yet been measured.

See [what works today](docs/implemented-baseline.md) and the
[roadmap](PLAN.md) for what remains.

## Where we want to get

A database where you can:

- Use familiar PostgreSQL SQL and client tools, with clear errors for anything
  unsupported.
- Trust a successful commit even if the server loses its local disk.
- Run reports and updates together without wrong results, with no slow storage
  request stopping unrelated work, using several CPU cores.
- Fix memory and disk limits before startup, share that capacity according to
  traffic, and get a named error when it runs out.
- Replace a failed server from object storage and reroute clients with a tested
  procedure.
- Judge speed and cost from repeatable measurements against PostgreSQL.

## Key choices

One active server writes to each object-store prefix: the set of object names
that holds one pos3ql instance and all its SQL databases.
[Replacement](docs/operations.md#controlled-replacement) moves write ownership to
another server.

**Where commits are saved** (`object_store`):

| Setting | Before a success response | If local disk is lost |
|---|---|---|
| `off` | The local recovery journal is synced to disk. | Local-only data is lost. |
| `on` | Transaction data is uploaded and the durable commit record is updated. | The database recovers from object storage. |

**How transactions share data** (`transaction_mode`, required):

- `mvcc` keeps older row values while readers need them. A repeatable-read
  report keeps seeing an order's earlier status while another transaction
  changes it.
- `locking` keeps only current committed values; conflicting work waits on
  relation locks. It supports read committed (and read uncommitted, which
  PostgreSQL treats the same). Repeatable read, serializable and exported
  snapshots return `0A000`.

Rollback and savepoints work in both modes. The mode is fixed for the server's
lifetime.

Every memory and capacity reservation is an explicit setting. The
[operations guide](docs/operations.md) explains them and
[architecture](docs/architecture.md#memory-and-physical-size) gives their limits.

## Getting started

Install a checksummed Linux release with the [installation guide](packaging/README.md).
With a development build, run:

```sh
pos3ql --config examples/dev.conf
psql -h 127.0.0.1 -p 5433 -U postgres -d postgres
```

Then in `psql`:

```sql
CREATE TABLE orders (id integer PRIMARY KEY, status text);
INSERT INTO orders VALUES (1, 'new');
SELECT status FROM orders WHERE id = 1;
```

This returns `new`. The development configuration saves data locally. To use
object storage, configure an endpoint, bucket, unique prefix, TLS and a
credential file readable only by its owner; see
[object storage](docs/object-storage.md).

## Documentation

The [documentation index](docs/README.md) lists every guide.
[CONTRIBUTING.md](CONTRIBUTING.md) explains development and checks.
[BUGS.md](BUGS.md) lists only defects blocked by a stated external reason.
License terms are in [LICENSE](LICENSE).

Copyright (c) Adrian Mârza and pos3ql contributors
