# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-12; object-store recovery, cancellation framing, garbage collection, binary cursor framing, catalog-aware decoding, binary record input, generated SQL comparison, catalog metadata, transactional DDL identities, durable expression-index keys, bounded index-cache reconstruction, login verification, publication and slot durability, pgoutput negotiation, logical-slot snapshots, volatile set-operation leaves, typed physical PAX demand through joins, correlation, materialization, group/window spill, unmatched outer joins, and DML source/`RETURNING` paths, corrupt PAX extent rejection and cold restart, target aliases, typed record expansion across derived and set-operation sources, ordering, grouping, wire descriptions, routine path resolution, typed SQL function formal propagation, mutable scalar/SETOF/TABLE routines including nested and LATERAL calls, final `RETURNING` and `void` actions, savepoints, WAL recovery, procedure programs, and TZif-only named-zone startup were audited.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
