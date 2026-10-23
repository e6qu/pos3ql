# Blocked defects

Only defects blocked by an external condition or a genuinely intractable repair
belong here. Fix every other defect, including CI regressions, in the PR that
finds it. Keep repaired defects and their evidence in the implementing PR.
Planned work and acceptance checks belong in [PLAN.md](PLAN.md).

No defects currently meet this blocked-only boundary.

Implemented guarantees belong in the [current behavior guide](docs/implemented-baseline.md)
and [storage format guide](docs/durable-format.md). Keep regression evidence in
the implementing PR.

Each entry must include a reproducer, affected boundary, observed behavior and
exact blocker. Schema waits, type retirement, publication, recovery, retained row
and table definitions, resumed command effects, storage ownership, slot reuse, column bindings and capacity
failures follow the same rule, including stale row layouts, temporary changes
sharing a journal position, irreversible commit failures and
checkpointing after partial installation. Fault tests must restart a terminally
failed instance before checking recovery; they must reject partial transactions.
Remove repaired entries; preserve their evidence in the implementing
change instead of keeping a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
