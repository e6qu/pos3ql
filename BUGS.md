# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-15. The CI suite covers object-store recovery, protocol framing, catalog-aware Bind/Result/COPY, typed collation persistence and compatibility, PostgreSQL-oracle SQL and type matrices, drivers, routine and trigger catalog durability and typed row-transition assignments, transactional subscription definitions, pgoutput publisher/apply/crash recovery, and cold physical-demand execution with explicit complete-row or proof-issued selected-column reads. Details belong in tests and git history, not this blocker register.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
