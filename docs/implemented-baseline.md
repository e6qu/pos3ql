# Implemented baseline

This summarizes current capabilities and verification mechanisms. Completion
requires the open gates in [PLAN.md](../PLAN.md); implemented mechanisms alone
do not qualify concurrent execution or representative deployments.

| Boundary | Implemented | Detailed contract |
|---|---|---|
| SQL and clients | PostgreSQL 18 command disposition, broad types and expressions, DDL/DML, MVCC, locks, savepoints, two-phase transactions, procedures, catalogs, drivers, and dump/restore | [Compatibility and capacities](postgresql-18-compatibility.md) |
| Durable storage | Immutable commits/checkpoints, durable response barriers, writer fencing, warm/cold caches, paced publication and deletion, mixed-format recovery | [Storage profile](object-storage.md), [formats](durable-format.md) |
| Physical indexes | Object-native btree, hash, BRIN, GiST, GIN, and SP-GiST paths, overlays, covering payloads, conservative pruning and exact rechecks | [Index navigation](index-navigation.md) |
| Recovery and operations | Named backups, independent-prefix export, LSN/time recovery, health/metrics/capacity, credential reload, packaged passive-candidate promotion | [Backup](backup-restore.md), [operations](operations.md) |
| Logical copies | PostgreSQL publisher/subscriber and pgoutput interoperability with bounded bootstrap/apply | [Logical replication](logical-replication.md) |
| Specialized SQL | Typed JSON/JSONB/jsonpath and SQL/JSON; XML and the documented XPath subset | [SQL/JSON](sql-json.md), [SQL/XML](sql-xml.md) |
| Verification | Allocation-forbidden tests, SQLSTATE and wire probes, drivers, PostgreSQL differential/regression corpora, cold recovery, storage VOPR, and performance-smoke gates | [Contributing](../CONTRIBUTING.md), [performance](performance.md) |

## Runtime ownership

| State | Current boundary |
|---|---|
| Statement execution | Startup-bounded arenas, DML scratch, backend/database context, and FIFO dispatcher leases; COPY/subscription state has separately charged buffers |
| Catalogs and metadata | Synchronized publication and owned reader images; guards release before nested resolution |
| Table metadata | Database/creation/owner identity, typed CREATE/DROP existence, definitions, and pending heads share guarded ownership; version slots are guarded, retained images capture identity with the definition, and publication remains exclusive |
| Serial positions | Per-table synchronization, coherent WAL/checkpoint images, checked arithmetic, and acknowledgement tied to unchanged staged positions |
| Resident rows | Per-table map guards and one guarded pending/committed version owner; issued readers retain chain ownership, including SQL and checkpoint walks; pending writes are immutable appends with checked identities and rollback tokens |
| Heap bytes | Readers pin published immutable ranges; appends initialize disjoint tails and publish complete bytes without excluding existing readers; relocation remains exclusive |
| Deferred rows | Logical row identity, table incarnation, and exact pending/committed version tokens; later reads reacquire the selected version rather than retaining heap locations |

Reader capacity, lock controls, and heap controls are charged at startup.
Exhaustion is explicit; readers reject reused table identities and stale heap
locations. Appends release byte ownership before publishing row metadata;
compaction preflights its complete relocation set before changing bytes or handles.
Heap locations and relocation generations are cache metadata, not durable row
identities. Retained byte copies consume the fixed statement arena; deferred
snapshots retain compact logical identities and exact version tokens.

The reactor still executes statements serially. Shared table lifecycle mutation
and retirement, row publication, concurrent cache/object I/O and maintenance,
engine publication, and fixed workers remain open gates. Existing snapshot
retention must remain coherent across those boundaries. Immutable SST reads
carry no resident version handles.

## Evidence limits

The performance harness includes the instrumented fixture, pinned local MinIO
and SeaweedFS, and host-available and resource-matched vanilla PostgreSQL 18.
Retained runs qualify mechanisms and exploratory costs. They do not establish
production latency, throughput, or multi-host availability.

Original implementation narratives and per-change observations are preserved
in [history](history/README.md). Current limits belong in the compatibility
matrix and specialized contracts above.
