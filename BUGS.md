# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-11; object-store recovery, cancellation framing, garbage collection, binary cursor framing, catalog-aware decoding, binary record input, array/composite wire fidelity, network value construction, generated SQL result comparison, catalog metadata and information-schema constraint/type/role/ACL/view/routine usage, transactional domain and routine identity across rename, schema move, dependent metadata, WAL recovery, checkpoints, bounded index-cache reconstruction, login verification states, publication definition staging, slot acknowledgement durability, pgoutput version negotiation, logical-slot snapshot handling, and TZif-only named-zone startup were audited.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
