# Find the guide you need

Start with the [overview](../README.md). The guides describe current behavior;
unfinished work lives only in the [roadmap](../PLAN.md).

| Your question | Guide |
|---|---|
| Can my application use this PostgreSQL feature? | [Compatibility and limits](postgresql-18-compatibility.md), [SQL/JSON](sql-json.md), [SQL/XML](sql-xml.md) |
| What can my application rely on today? | [What works today](implemented-baseline.md) |
| How do requests, commits, storage layout and caches work? | [Architecture](architecture.md) |
| What are the memory and size limits? | [Physical limits](architecture.md#memory-and-physical-size) |
| How do I install, configure, monitor or replace a server? | [Installation](../packaging/README.md), [operations](operations.md) |
| How do scheduled tasks and migrations run? | [Scheduling and migrations](operations.md#scheduled-work-and-schema-migrations) |
| What happens if the local disk or object store fails? | [Object storage](object-storage.md) |
| How do I back up, restore or copy a database? | [Backup and restore](backup-restore.md), [logical replication](logical-replication.md) |
| Will an upgrade read my existing data? | [Durable formats](durable-format.md) |
| How do indexes find rows in saved files? | [Index navigation](index-navigation.md) |
| How fast is it, and how do I measure? | [Performance](performance.md), [retained benchmarks](../benchmarks/README.md) |
| How will capacity sharing and feedback control work? | [Resource design](adaptive-resources.md) (unfinished) |
| What does a term mean? | [Glossary](terminology.md) |
| How do I contribute? | [Contributing](../CONTRIBUTING.md), [engineering rules](../AGENTS.md) |
| Which defects are blocked? | [Blocked defects](../BUGS.md) |

## Evidence and provenance

- [Retained benchmarks](../benchmarks/README.md): dated raw results and reports.
- [Implementation history](history/README.md): earlier plans and completed
  investigations at fixed revisions.
- Fixture generation: `tests/data/README.md`.
- Third-party tests and checksums: `vendor/README.md` and `vendor/SHA256SUMS`.
- Compiled-in third-party code and data, the Snowball stemmers and stopword
  lists: [Snowball provenance](postgresql-18-compatibility.md#snowball-provenance).
