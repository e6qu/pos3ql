# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-09; object-store recovery, cancellation framing, garbage collection, binary cursor framing, catalog-aware decoding, binary record input, array/composite wire fidelity, network value construction, generated SQL, catalog metadata, ACL introspection, typed column-default and domain-definition staging, login verification states, publication definition staging, identity, ownership, and schema selection, slot acknowledgement durability, pgoutput version negotiation, logical-slot snapshot handling, and TZif-only named-zone startup were audited.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
