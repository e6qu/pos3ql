# Blocked defects

Only defects blocked by an external condition or a genuinely intractable repair
belong here. Fix every other defect, including CI regressions, in the PR that
finds it. Keep repaired defects and their evidence in the implementing PR.
Planned work and acceptance checks belong in [PLAN.md](PLAN.md).

No defects currently meet this blocked-only boundary.

A configured capacity limit may reject work explicitly. Partial changes, lost
reader data or leaked ownership after that rejection are defects to fix here.

Each entry must include a reproducer, affected boundary, observed behavior and
exact blocker. Publication, recovery and capacity failures follow the same rule.
Remove repaired entries; preserve their evidence in the implementing
change instead of keeping a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
