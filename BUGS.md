# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix reproducible defects in the PR that finds them, including incidental defects.
Record an entry here only with its reproducer, observed behavior and the exact
external blocker or reason it cannot be repaired. Ordinary regression cases
and resolved investigations belong in their fixing PR, tests and evidence.

Follow [CONTRIBUTING.md](CONTRIBUTING.md) for the required comparisons,
allocation checks and failure diagnostics. Regression details belong in their
owning tests and technical guides, rather than a second checklist here.

Unfinished features and their acceptance criteria belong in [PLAN.md](PLAN.md).
Current guarantees belong in the [behavior guide](docs/implemented-baseline.md)
and its [owning technical guides](docs/README.md).

Successful-call regressions must compare successful values. Matching errors on
both servers do not establish support; keep SQL and binary-client checks aligned.
Argument resumption checks must also compare non-sequence effects, changed
session settings, selected defaults and replacement timing with PostgreSQL.
Record expansion checks must also compare argument effects and declared field
types, including independent calls and shared set-returning producers.
Keep small-arena index checks in the full gate: repeated nonsuspending builtin
expressions must not accumulate routine input records. Compare anonymous-record
casts with PostgreSQL before using a named type to persist function results.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
