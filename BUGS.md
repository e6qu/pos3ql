# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix reproducible defects in the PR that finds them, including incidental
failures, invalid catalog or column identities, incorrect report types,
database names or permissions, timeouts, rollback and recovery errors.
Malformed expression input must fail before it can become a stored catalog node.
Record a defect here only when an external blocker or genuine intractability
prevents its repair.
Each entry needs a reproducer, affected behavior, observed result and exact
blocker. Remove repaired entries and retain their evidence in the fixing PR.

Unfinished features and acceptance criteria belong in [PLAN.md](PLAN.md).
Current guarantees belong in the [behavior guide](docs/implemented-baseline.md)
and [storage format guide](docs/durable-format.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
