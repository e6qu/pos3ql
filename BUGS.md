# Blocked defects

Only defects blocked by an external condition or a genuinely intractable repair
belong here. Fix every other defect, including CI regressions, in the PR that
finds it. Keep repaired defects and their evidence in the implementing PR.
Planned work and acceptance checks belong in [PLAN.md](PLAN.md).

No defects currently meet this blocked-only boundary.

A configured capacity limit may reject work explicitly. Partial changes, lost
reader data, hidden runtime allocation or leaked ownership are defects. Waiting,
rollback, connection identity, permission checks, ownership across databases,
recovery and slot reuse must preserve valid data and selected source identities.
An unfinished metadata reference must prevent account deletion until its
transaction commits or rolls back, including after prepared recovery.
View, materialized-view, rule, sequence, routine and table-drop undo must not change a replacement
object or its comments. Materialized-view changes retain the backing relation's
lifetime as well. A failed undo-log attachment must release the staged
change; identity restarts use the same ownership checks as sequence edits.
Recovery must preserve saved catalog positions and creation identities, including
gaps left by rolled-back creations. Prepared sequence restarts retain their
private values; rollback must preserve the committed counter.
Materialized-view readability follows transaction and savepoint rollback.
A pending login commit must yield without repeating triggers. Synchronous
journal completion must not wait for credential installation by its own caller.
Keep detailed contracts in the
[current behavior guide](docs/implemented-baseline.md) and regression evidence in
the implementing PR.

Each entry must include a reproducer, affected boundary, observed behavior and
exact blocker. Publication, recovery and capacity failures follow the same rule.
Remove repaired entries; preserve their evidence in the implementing
change instead of keeping a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
