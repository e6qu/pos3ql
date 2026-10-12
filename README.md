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

**Queries still execute one at a time in each server process.** The recent
work protects data that a reader still needs while other work changes it.
That is preparation for concurrent execution. We have not yet enabled several
queries to execute together or demonstrated production performance and failover
on separate machines.

For example, you can connect with `psql`, save a row, and recover it from object
storage after losing both local caches. We still need to show that a slow report
fetching uncached data lets another customer update an unrelated order promptly.

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

The storage layout should make useful reads small and avoid rewriting unchanged
data. For example, reading two columns of a wide table should not require
downloading every column. [Storage layout](docs/durable-format.md) and
[indexes](docs/index-navigation.md) describe the existing mechanisms; the
[performance guide](docs/performance.md) explains how we measure their effect.

One active server writes to each configured object-store prefix, the group of
object names belonging to that database. The goal is concurrent transactions
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

The [documentation index](docs/README.md) routes questions about compatibility,
running the server, backups, storage, and performance. [CONTRIBUTING.md](CONTRIBUTING.md)
explains development and checks. [BUGS.md](BUGS.md) lists only known defects
that cannot currently be fixed because of a stated blocker; it is not a list
of all unfinished work. License terms are in [LICENSE](LICENSE).

Copyright (c) Adrian Mârza and pos3ql contributors
