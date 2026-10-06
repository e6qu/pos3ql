# Documentation index

## Current guidance

| Document | Owns |
|---|---|
| [Project overview](../README.md) | Architecture, status, and first run |
| [Roadmap](../PLAN.md) | Remaining sequence, gaps, and completion criteria |
| [Implemented baseline](implemented-baseline.md) | Summary of current capabilities |
| [PostgreSQL 18 compatibility](postgresql-18-compatibility.md) | SQL/wire scope, capacity inventory, and non-goals |
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
- [Implementation history](history/README.md): immutable pre-review records.
- Fixture generation: `tests/data/README.md` in the source repository.
- Upstream corpora and checksums: `vendor/README.md` and `vendor/SHA256SUMS`.

Keep current behavior in its owning document. Preserve original benchmark
artifacts and upstream provenance; link them rather than rewriting observations
as current production claims.
