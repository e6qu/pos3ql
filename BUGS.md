# Blocked defects

No defects currently meet the reporting boundary below.

PR #705 keeps the catalog, cloning, vector recovery, and allocation-check
regressions in its implementation and test evidence until its full gates pass.
Guarded recovery tests retain both the original request and resumed commit
checks while simulated remote publication advances between request turns.
The table-column rename regression remains in the same PR, with source-bound
rewrite and recovery coverage rather than a deferred bug entry.

Fix reproducible defects in the PR that finds them, including incidental
failures, timeouts, rollback and recovery errors. Record a defect here only
when an external blocker or genuine intractability prevents its repair.
Each entry needs a reproducer, affected behavior, observed result and exact
blocker. Remove repaired entries and retain their evidence in the fixing PR.

Unfinished features and acceptance criteria belong in [PLAN.md](PLAN.md).
Current guarantees belong in the [behavior guide](docs/implemented-baseline.md)
and [storage format guide](docs/durable-format.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
