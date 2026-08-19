# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-19. The CI suite covers object-store recovery, protocol framing, catalog-aware Bind/Result/COPY including direct and domain enum/composite arrays, PostgreSQL-oracle SQL and type matrices, outbound pg_dump/restore, drivers, durable routine and trigger lifecycles, transactional subscriptions, pgoutput recovery, named-type identity moves, and cold physical-demand execution. Details belong in tests and git history, not this blocker register.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
