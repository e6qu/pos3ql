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
| Resident rows | Shared pending publication and exact-head rollback preserve pinned readers and committed deletion markers. Metadata, uniqueness, optimized byte, and resumable value-index checkpoint walks release row ownership before consuming selected bytes; committed publication and reclamation remain exclusive |
| Heap bytes | Readers pin published immutable ranges; appends initialize disjoint tails and publish complete bytes without excluding existing readers; relocation remains exclusive |
| Deferred rows | Logical row identity, table incarnation, and exact pending/committed version tokens; later reads reacquire the selected version rather than retaining heap locations |

Reader capacity, lock controls, and heap controls are charged at startup.
Authoritative metadata walks and internal byte scans share 64 retention slots
per query workspace, sized for the larger of the table and large-object page
overlays. SQL byte scans use the fixed statement arena, preserving accepted
wide joins without consuming a metadata slot for each edge. Both paths freeze
one overlay partition through SST merging; outer joins track physical row
identities across scan orders. Capacity exhaustion reports SQLSTATE 54000.

Resumable value-index checkpoint sources charge one table_rows identity/coverage buffer
at startup. Bucket movement and pending rollback do not move their logical
resume position; reproducible eviction routes unprocessed rows through the paced SST merge while
preserving coverage for emitted resident rows.
Selected committed images are pinned before releasing row metadata. Checkpoint
SST sources retain their committed boundary, immutable handles, schema, and shared
read pool across beats. A startup-sized registry protects their durable or local
temporary block graphs during retirement; garbage collection copies roots before
object I/O. Releasing a root retained by a durable garbage pass schedules another
maintenance pass without requiring a new write. Root and reader-count exhaustion
reports SQLSTATE 54000 before changing registrations.

Checkpoint jobs restart when committed input or table identity changes. Query
scans still retain table incarnations through their Storage borrow; concurrent
query retirement and generation publication remain open gates.

Appends release byte ownership before publishing row metadata. Compaction
preflights its relocation set before changing bytes or handles; readers reject
reused table identities and stale heap locations. Heap locations and relocation
generations are cache metadata. Retained byte copies use the statement arena;
deferred snapshots retain logical row identities and exact version tokens.

The reactor still executes statements serially. Shared table lifecycle mutation
and retirement, committed row publication, concurrent cache/object I/O and maintenance,
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
