# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix every reproducible defect in the PR that finds it. Record an entry only
with its reproducer, observed behavior and exact external blocker or reason it
cannot be repaired. Keep reproductions, regression checks and PostgreSQL
comparisons in the fixing PR, including declared result types, input lifetimes,
argument and sequence effects, and when errors occur.
Preserve failed outputs and diagnostics before removing temporary fixtures.
See [CONTRIBUTING.md](CONTRIBUTING.md).

Unfinished features and their verification criteria belong in [PLAN.md](PLAN.md).
Current guarantees belong in the [behavior guide](docs/implemented-baseline.md)
and its [owning technical guides](docs/README.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
