# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-10; object-store recovery, cancellation framing, garbage collection, binary cursor framing, catalog-aware decoding, binary record input, array/composite wire fidelity, network value construction, generated SQL result comparison, catalog metadata and information-schema constraint/type/role/ACL/view/routine usage, typed intrinsic and durable routine OID metadata, transaction-owned column/default/domain/enum/sequence/schema/view/routine definitions, login verification states, publication definition staging, slot acknowledgement durability, pgoutput version negotiation, logical-slot snapshot handling, and TZif-only named-zone startup were audited.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
