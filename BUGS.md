# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-11; object-store recovery, cancellation framing, garbage collection, binary cursor framing, catalog-aware decoding, binary record input, generated SQL comparison, catalog metadata, transactional DDL identities and partial-index membership through WAL and checkpoints, bounded index-cache reconstruction, login verification, publication and slot durability, pgoutput negotiation, logical-slot snapshots, and TZif-only named-zone startup were audited.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
