# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-30 against PostgreSQL 18 across query semantics and bounded materialization, full-text values and catalog lifecycles, two-phase transactions, rewrite-rule authorization and execution, DDL and event-trigger command/drop graphs, locale and encoding objects, transactional table/index evolution, database and tablespace isolation, session and transaction configuration, routines, casts/operators, privileges, accepted types, portals, drivers, COPY, dump/restore, WAL, checkpoints, and provider-neutral cold recovery. Details belong in tests and git history, not this blocker register.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
