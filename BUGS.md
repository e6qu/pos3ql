# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix reproducible defects in the PR that finds them, including incidental defects
and resource-ownership failures.
Record an entry here only with its reproducer, observed behavior and exact
external blocker or reason it cannot be repaired. Keep resolved investigations
with their fixing code, tests and retained reference evidence.

Pending CI, uncollected measurements and reproducible failures being repaired
belong to the active PR and [PLAN.md](PLAN.md). They are not blocked defects.
[CONTRIBUTING.md](CONTRIBUTING.md) owns verification and failure diagnostics;
current behavior and limits belong in the [owning guides](docs/README.md).
Fixing tests and retained PostgreSQL reference evidence own the exact SQL cases,
including parameter/local/output scopes, constant targets, cursor behavior and
pending-call argument/type ownership and completed-result ownership.
Saved-input images and publication while query scratch borrows the parent arena
have allocation and ownership regressions in the active PR.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
