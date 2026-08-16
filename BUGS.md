# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-17. The CI suite covers object-store recovery, protocol framing, catalog-aware Bind/Result/COPY, typed collation persistence, PostgreSQL-oracle SQL and type matrices, drivers, durable trigger lifecycles including `OLD`/`NEW`, `TG_*` context, local programs, diagnostics, control flow, DML/`MERGE`, transactional subscriptions, pgoutput recovery, and cold physical-demand execution across joins, correlation, materialization, windows, aggregates, and source DML. Details belong in tests and git history, not this blocker register.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
