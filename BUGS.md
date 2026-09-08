# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-09-08 against the complete 183-command PostgreSQL 18 inventory, PostgreSQL 18 SQL/JSON and jsonpath, foreign-table MERGE, lazy foreign-session savepoint mirroring, maintenance-target inheritance selection, catalog-atomic native-hook rejection, relation persistence and session teardown, bounded local temporary spill, exact clean/crash recovery, temporary WAL/checkpoint/publication exclusion, typed foreign-session ownership, outbound Bind framing, routine and PL/pgSQL execution, roles and privileges, operators and extensions, PostgreSQL 18.6 publisher/subscriber and `pg_recvlogical` interoperability, logical bootstrap catalog introspection, transactional and nontransactional logical messages, typed slot-management and monitoring surfaces, strict pgoutput tuple widths and framing, wire drivers, object-store recovery, and pg_dump/restore. Unsupported PostgreSQL behavior is an explicit typed boundary, not deferred work. Details belong in tests and git history, not this blocker register.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
