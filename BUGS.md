# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix every reproducible defect in the PR that finds it.
Record an entry only with its reproducer, observed behavior and exact blocker;
retain repaired-defect evidence in the fixing PR. See [CONTRIBUTING.md](CONTRIBUTING.md).

Unfinished features and their verification criteria belong in [PLAN.md](PLAN.md).
Current guarantees, including catalog identities, transactional session cleanup
and recovery checks, belong in
the [behavior guide](docs/implemented-baseline.md),
[architecture and physical limits](docs/architecture.md), and
[storage format guide](docs/durable-format.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
