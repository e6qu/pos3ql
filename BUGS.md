# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix reproducible defects in the PR that finds them, including incidental defects
and failures during admission, initialization, suspension, restoration, nested
writes, trigger dispatch, mixed-operation transition tables, nested command
restoration, borrowed-memory admission, routine-setting scopes, binary-value
rendering, readable-setting completeness or cleanup.
Firing capture and attachment include arguments, row values and selected column
descriptions, configured routine settings, effective user, original timestamps
and deadline as well as lexical execution state. Isolated ownership tests use an
explicitly scoped clock for real lock waits; they keep interpreter isolation enabled.
Record an entry here only with its reproducer, observed behavior and exact
external blocker or reason it cannot be repaired. Keep resolved investigations
with their fixing code, tests and retained reference evidence.

Pending CI, uncollected measurements and reproducible failures being repaired
belong to the active PR and [PLAN.md](PLAN.md). They are not blocked defects.
[CONTRIBUTING.md](CONTRIBUTING.md) owns verification and failure diagnostics;
current behavior and limits belong in the [owning guides](docs/README.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
