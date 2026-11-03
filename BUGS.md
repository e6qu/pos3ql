# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix reproducible defects in the PR that finds them, including incidental defects.
Record an entry here only with its reproducer, observed behavior and the exact
external blocker or reason it cannot be repaired. Keep resolved investigations
with their fixing code, tests and retained reference evidence.

[CONTRIBUTING.md](CONTRIBUTING.md) defines the required comparisons, allocation
checks and failure diagnostics. Repair causes while preserving the original
capacity limits and legitimate lock waits. Compare SQL values, effects, errors
and client-message boundaries with actual PostgreSQL; compiler success alone
does not establish runtime correctness. Recovery tests must exercise their
declared durable tier before deleting caches.
Pending CI and uncollected measurements are acceptance work in [PLAN.md](PLAN.md),
not blocked defects. Current behavior belongs in the [behavior guide](docs/implemented-baseline.md)
and its [owning technical guides](docs/README.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
