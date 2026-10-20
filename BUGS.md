# Blocked defects

Only defects blocked by an external condition or a genuinely intractable repair
belong here. Fix every other defect, including CI regressions, in the PR that
finds it, including publication that exposes a renamed type before its dependent
definitions or keeps earlier owners locked while waiting.
Planned work and acceptance checks belong in [PLAN.md](PLAN.md).

No externally blocked defects are currently recorded.

Each entry must include a reproducer, affected boundary, observed behavior and
exact blocker. Remove repaired entries; preserve their evidence in the implementing
change instead of keeping a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
