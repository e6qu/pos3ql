# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-31 against PostgreSQL 18 across query semantics and bounded materialization, foreign-data catalogs and scans, large objects, full-text values, two-phase transactions, rewrite and event triggers, locale and encoding objects, table/index attachment and generated/identity evolution, replica identity and pgoutput, database and tablespace isolation, configuration, routines, casts/operators, privileges, accepted types, portals, drivers, COPY, dump/restore, WAL, checkpoints, and provider-neutral cold recovery. Unsupported table lifecycle forms are explicit typed boundaries, not deferred bugs. Details belong in tests and git history, not this blocker register.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
