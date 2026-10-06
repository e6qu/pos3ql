# Implemented baseline

Current implementation summary after PR #599 (2026-10-06). This describes
capabilities and verification mechanisms, not completion of the production
roadmap. Outstanding work is in [PLAN.md](../PLAN.md).

| Boundary | Implemented | Detailed contract |
|---|---|---|
| SQL and clients | PostgreSQL 18 command disposition, broad types and expressions, DDL/DML, MVCC, locks, savepoints, two-phase transactions, procedures, catalogs, drivers, and dump/restore | [Compatibility and capacities](postgresql-18-compatibility.md) |
| Durable storage | Immutable commits/checkpoints, durable response barriers, writer fencing, warm/cold caches, paced publication and deletion, mixed-format recovery | [Storage profile](object-storage.md), [formats](durable-format.md) |
| Physical indexes | Object-native btree, hash, BRIN, GiST, GIN, and SP-GiST paths, overlays, covering payloads, conservative pruning and exact rechecks | [Index navigation](index-navigation.md) |
| Recovery and operations | Named backups, independent-prefix export, LSN/time recovery, health/metrics/capacity, credential reload, packaged passive-candidate promotion | [Backup](backup-restore.md), [operations](operations.md) |
| Logical copies | PostgreSQL publisher/subscriber and pgoutput interoperability with bounded bootstrap/apply | [Logical replication](logical-replication.md) |
| Specialized SQL | Typed JSON/JSONB/jsonpath and SQL/JSON; XML and the documented XPath subset | [SQL/JSON](sql-json.md), [SQL/XML](sql-xml.md) |
| Verification | Allocation-forbidden tests, SQLSTATE and wire probes, drivers, PostgreSQL differential/regression corpora, cold recovery, storage VOPR, and performance-smoke gates | [Contributing](../CONTRIBUTING.md), [performance](performance.md) |

## Concurrency preparation

Statement arenas, DML selection scratch, backend/database context, and dispatcher
leases are startup-bounded and isolated by execution workspace. Long-lived
client COPY and subscription state have their own charged buffers.

Catalog and metadata publication now has synchronized boundaries for types,
sequences, authorization, routines, triggers, policies, views, rewrite rules,
dependencies, indexes, database identities,
system settings, prepared catalog images, and active cluster defaults. Owned
reader images release guards before nested resolution. The latest database and
cluster changes cover pending identity reservation, retirement, duplicate
recovery records, coherent snapshots, and exact retained capacity.

The reactor still executes statements serially. Table definitions and row
mutation, engine-owned prepared slots and WAL publication, fixed workers, and
scaling qualification remain open. Catalog metadata synchronization does not
make engine execution concurrent.

## Evidence limits

The performance harness includes the instrumented fixture, pinned local MinIO
and SeaweedFS, and host-available and resource-matched vanilla PostgreSQL 18.
Retained runs qualify mechanisms and exploratory costs. They do not establish
production latency, throughput, or multi-host availability.

Original implementation narratives and per-change observations are preserved
in [history](history/README.md). Current limits belong in the compatibility
matrix and specialized contracts above.
