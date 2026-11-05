# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix reproducible defects in the PR that finds them, including incidental defects.
Record an entry here only with its reproducer, observed behavior and exact
external blocker or reason it cannot be repaired. Keep resolved investigations
with their fixing code, tests and retained reference evidence.

Pending CI, uncollected measurements and reproducible failures being repaired
belong to the active PR and [PLAN.md](PLAN.md). They are not blocked defects.
[CONTRIBUTING.md](CONTRIBUTING.md) owns verification and failure diagnostics;
current behavior belongs in the [owning guides](docs/README.md). Preserve
regression coverage when changing ownership or execution: successful results
alone cannot prove that effects ran once, constraints saw the right rows, or
temporary references were released safely.
Function regressions must distinguish how many columns a call returns from how
many rows it returns; selecting output names in `FROM` exercises both contracts.
Include named rows returned as separate fields, whole values and nested values,
and verify that constraint helpers run once during result conversion.
Memory regressions must repeat queries and nested calls at unchanged capacity,
including nested results on every iteration;
successful short loops cannot establish that discarded work is reclaimed.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
