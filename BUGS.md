# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix reproducible defects in the PR that finds them, including incidental defects.
Record an entry here only with its reproducer, observed behavior and exact
external blocker or reason it cannot be repaired. Keep resolved investigations
with their fixing code, tests and retained reference evidence.

Pending CI, uncollected measurements and reproducible failures being repaired
belong to the active PR and [PLAN.md](PLAN.md). They are not blocked defects.
[CONTRIBUTING.md](CONTRIBUTING.md) owns verification and failure diagnostics;
current behavior belongs in the [owning guides](docs/README.md). Verify catalog
identities, comments and physical flags before and after maintenance and cold
recovery; command completion alone cannot expose duplicate or detached index
metadata. Preserve failing-thread stacks with native-crash repairs.
Exercise constraint-index DDL before a rebuild and preserve retired-index owners
through table-owner changes and recovery.
Verify same-lifetime key/index name aliases separately from real collisions,
and exercise maintenance whose requested index state already holds.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
