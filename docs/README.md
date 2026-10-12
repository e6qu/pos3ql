# Find the guide you need

Start with the [overview](../README.md) for where the project is today and the
[plan](../PLAN.md) for what remains. The guides below describe current behavior;
an example of a future goal is labeled as remaining work in the plan.

| Your question | Start here |
|---|---|
| Can my application use this PostgreSQL feature? | [Compatibility and limits](postgresql-18-compatibility.md) |
| How do I install, monitor, or replace a server? | [Installation](../packaging/README.md), [operations](operations.md) |
| How do I recover an earlier state or copy the database? | [Backup and restore](backup-restore.md) |
| What happens if the local disk or object store fails? | [Object storage](object-storage.md) |
| Will an upgrade read my existing data? | [Durable formats](durable-format.md) |
| How do I compare speed and resource use with PostgreSQL? | [Performance](performance.md) |
| What does an unfamiliar term mean? | [Glossary](terminology.md) |

## Current guidance

| Document | What it explains |
|---|---|
| [Project overview](../README.md) | Purpose, status, destination, and first run |
| [Roadmap](../PLAN.md) | Remaining work, its order, and how we know it is done |
| [What works today](implemented-baseline.md) | Current behavior and concurrency preparation |
| [PostgreSQL 18 compatibility](postgresql-18-compatibility.md) | Supported behavior, capacity limits, and features outside the design |
| [Contributing](../CONTRIBUTING.md) | Development, verification, and PR workflow |
| [Agent rules](../AGENTS.md) | Required engineering and change rules |
| [Terminology](terminology.md) | Naming and glossary |
| [Blocked defects](../BUGS.md) | Genuine blockers to repairing known bugs |

## Operations and internals

- [Installation](../packaging/README.md)
- [Operations, credentials, readiness, and replacement](operations.md)
- [Backup, export, and point-in-time recovery](backup-restore.md)
- [S3-compatible storage profile and qualification](object-storage.md)
- [Durable formats and migration](durable-format.md)
- [Immutable index navigation](index-navigation.md)
- [Performance harness and interpretation](performance.md)

## Specialized compatibility

- [Logical replication](logical-replication.md)
- [SQL/JSON](sql-json.md)
- [SQL/XML and XPath](sql-xml.md)

## Evidence and provenance

- [Retained benchmarks](../benchmarks/README.md): dated raw artifacts and reports.
- [Implementation history](history/README.md): immutable implementation records and completed investigations.
- Fixture generation: `tests/data/README.md` in the source repository.
- Original third-party tests and checksums: `vendor/README.md` and `vendor/SHA256SUMS`.

Keep current behavior in the guide that explains it and unfinished work in
PLAN.md. Preserve original measurements, third-party sources, and completed
investigations; their dates and revisions matter when interpreting a result.
