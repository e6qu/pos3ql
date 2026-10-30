# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix every reproducible defect in the PR that finds it. Record an entry only
with its reproducer, observed behavior and exact external blocker or reason it
cannot be repaired. Keep repaired-defect evidence, ownership checks and
PostgreSQL comparisons in the fixing PR. Cover deferred expression errors and row bounds, scroll-history exhaustion,
frozen operator choices, savepoint read
accounting and table-slot reuse, as well as exhausted memory before
irreversible effects, owner moves, rollback and suspended queries. Preserve
failed-comparison outputs and diagnostics before removing temporary files.
See [CONTRIBUTING.md](CONTRIBUTING.md).

Unfinished features and their verification criteria belong in [PLAN.md](PLAN.md).
Current guarantees belong in the [behavior guide](docs/implemented-baseline.md)
and its [owning technical guides](docs/README.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
