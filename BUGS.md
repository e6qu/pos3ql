# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-20 after COPY FROM preserved PostgreSQL 18 header/default/error-control semantics across its typed setup and streamed protocol rows, while custom text delimiters and NULL sentinels reached the decoder. Golden `psql` validation compares stdout and stderr independently; CI uses runner `psql` for ordinary SQL, PostgreSQL 18 `psql` for protocol meta-commands, and PostgreSQL 18 image tools for dump/restore, without a package mirror. The suite covers object-store recovery, protocol framing, catalog-aware Bind/Result/COPY, PostgreSQL-oracle SQL and type matrices, binary portal paging, dump/restore, drivers, durable routines/triggers, transactional subscriptions, pgoutput recovery, named-type identity moves, and cold physical-demand execution. Details belong in tests and git history, not this blocker register.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
