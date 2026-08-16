# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-16. The CI suite covers object-store recovery, protocol framing, catalog-aware Bind/Result/COPY, typed collation persistence, PostgreSQL-oracle SQL and type matrices, drivers, durable trigger lifecycles including `OLD`/`NEW`, `TG_*` context, local programs, `FOUND`, `GET [CURRENT] DIAGNOSTICS` and stacked diagnostics, control flow, all non-error `RAISE` severities, validated custom `SQLSTATE`/condition/`ERRCODE` and `USING`, handler-only rethrow, strict `SELECT INTO`, rollback handlers, transition sources, DML, `TRUNCATE`, and `MERGE`, transactional subscription definitions, pgoutput publisher/apply/crash recovery, and cold physical-demand execution. Details belong in tests and git history, not this blocker register.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
