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
current behavior belongs in the [owning guides](docs/README.md), including
retained caught-error ownership, initializer-free runtime local metadata,
rewritten assignment inputs, quoted procedural names, typed trigger context
and PostgreSQL stacked-diagnostic values. Output-parameter initializer indexing
and named `undefined_column` handlers are covered by the active regressions,
alongside quoted cursor names and consistent lexical variable bindings.
The trigger audit uses the PostgreSQL-verified SQL fixture directly.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
