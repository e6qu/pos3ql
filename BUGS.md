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
When testing a trigger wait, compare the rows visible to its constraint as well
as final rows, audit writes and sequence counts. A rollback-and-replay attempt
can return the expected result while showing the constraint the wrong data.
Caught errors must also restore schema, journal, settings and deferred work;
test cursor creation separately from the motion of an older cursor.
Open foreign connections inside nested exception blocks too: a savepoint list
captured only at command admission misses boundaries created within that command.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
