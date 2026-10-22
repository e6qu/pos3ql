# Blocked defects

Only defects blocked by an external condition or a genuinely intractable repair
belong here. Fix every other defect, including CI regressions, in the PR that
finds it. Keep repaired defects and their evidence in the implementing PR.
Planned work and acceptance checks belong in [PLAN.md](PLAN.md).

The repair below is waiting for explicit approval after automatic review rejected
the durable recovery change. Its failing regression remains enabled.

A configured capacity limit may reject work explicitly. Partial changes, lost
reader data, hidden runtime allocation or leaked ownership are defects. Waiting,
rollback, connection identity, permission checks, ownership across databases,
recovery and slot reuse must preserve valid data and selected source identities.
An unfinished metadata reference must prevent account deletion until its
transaction commits or rolls back, including after prepared recovery.
View and rule undo must not change a replacement object or its comments.
Materialized-view readability follows transaction and savepoint rollback.
Keep detailed contracts in the
[current behavior guide](docs/implemented-baseline.md) and regression evidence in
the implementing PR.

Each entry must include a reproducer, affected boundary, observed behavior and
exact blocker. Publication, recovery and capacity failures follow the same rule.
Remove repaired entries; preserve their evidence in the implementing
change instead of keeping a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
| B-VIEW-IDENTITY | blocked | 2026-10-22 | Cold recovery changes the internal `_RETURN` rule identifier. | `query_lifecycle_view_identity_and_refresh_state_survive_disposable_disk_recovery`; focused run `39725671874` on `2f17d750`: rule identifier `26003` becomes `26016`. | Automatic approval review rejected the durable identity placement refactor because an incorrect change could corrupt catalog recovery; user approval is pending. This PR must pass the retained regression before merging. |
