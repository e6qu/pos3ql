# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix reproducible defects in the PR that finds them, including incidental defects.
Record an entry here only with its reproducer, observed behavior and the exact
external blocker or reason it cannot be repaired. Capacity, ownership and recovery
regressions, statistics and generator ownership, query-source binding failures,
prepared-input effects and resolved investigations belong
in their fixing PR, tests and evidence.

Follow [CONTRIBUTING.md](CONTRIBUTING.md) for the required comparisons,
allocation checks and failure diagnostics. Regression details belong in their
owning tests and technical guides, rather than a second checklist here.

Unfinished resource policies and feature goals, with their acceptance criteria,
belong in [PLAN.md](PLAN.md). Pending CI and uncollected measurements are
acceptance work, not blocked defects.
Repeated failures across native, coverage and PostgreSQL comparison jobs must
be traced to their shared cause and repaired before acceptance.
Current guarantees belong in the [behavior guide](docs/implemented-baseline.md)
and its [owning technical guides](docs/README.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
