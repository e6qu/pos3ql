# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-08-09; object-store recovery, cancellation framing, garbage collection, binary cursor framing, catalog-aware decoding, array/composite wire fidelity, network value construction, and test-only code were audited. Generated SQL now gates unsupported statements and covers array-subquery result metadata.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|
