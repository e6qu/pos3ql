# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix reproducible defects in the PR that finds them, including incidental defects.
Record an entry here only with its reproducer, observed behavior and the exact
external blocker or reason it cannot be repaired. Keep resolved investigations
with their fixing code, tests and retained reference evidence.

[CONTRIBUTING.md](CONTRIBUTING.md) defines the required comparisons, allocation
checks and failure diagnostics. Preserve ownership and allocation guards when
repairing tests. Compare the same client-message and transaction boundaries;
compiler success alone does not establish runtime correctness. Keep native
expectations tied to actual PostgreSQL observations, including planner effects
and cancellation followed by a fresh statement. Repair temporary-memory leaks
by releasing their owners; retain regressions at the original arena limits.
Preserve distinct function effects and caught errors through retries; validate
the original bounded-row, cursor, driver and recovery cases before merging.
Keep checked values through reparsing and default conversion; use declared
type identities rather than repeating a conversion that already completed.
Use PostgreSQL's canonical object labels across DDL events, identification and
address lookup; retain reference output for same-owner commands as well.
Exclude the destination role from ownership-reassignment work after permission
checks; same-owner changes must not loop or consume catalog capacity.
Preserve implicit-command versus executed-block state and clear failed protocol
work at the shared error boundary. Aggregate support-call authorization must not
change the role observed inside its body or permit direct unauthorized calls.
Transaction handoffs must preserve the waiting frontend's plan owner and release
free-worker ownership. Keep legitimate lock waits in regressions and resume the
same suspended state; distinguish those waits from lost state or repeated effects.
Preserve PostgreSQL's completed RETURN QUERY rows after caught SQL errors while
preventing duplicates across private retries. Recovery fixtures must enable and
exercise their declared durable tier before deleting caches.
Pending CI and uncollected measurements are acceptance work in [PLAN.md](PLAN.md),
not blocked defects. Current behavior belongs in the [behavior guide](docs/implemented-baseline.md)
and its [owning technical guides](docs/README.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
