# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-09-07 against typed foreign-session ownership, outbound Bind framing, remote INSERT/TRUNCATE transaction boundaries, PL/pgSQL declarations, DML row assignments, MERGE defaults, identity overrides and sequence defaults, inheritance leaves and recovery, publication descendant selection and privileges, subscription lifecycles, URI conninfo, pgoutput origins, WAL ordering, checkpoints, object-store recovery, and pg_dump/restore. Unsupported PostgreSQL behavior is an explicit typed boundary, not deferred work. Details belong in tests and git history, not this blocker register.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
