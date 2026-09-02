# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-09-02 against PostgreSQL 18 across SQL semantics, DDL, catalogs, PL/pgSQL scalar, record, and set execution, wire behavior, drivers, COPY, typed tables, partition constraints, publication and subscription lifecycles, logical replication, WAL, checkpoints, provider-neutral cold recovery, and bounded CI execution. Unsupported PostgreSQL behavior is an explicit typed boundary, not deferred work. Details belong in tests and git history, not this blocker register.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
