# Blocked defects

Record only defects whose repair is prevented by an external blocker or genuine
intractability. Fix every fixable defect in the PR that finds it. Planned work
and incomplete architecture belong in [PLAN.md](PLAN.md).

No externally blocked defects are currently recorded. Fixable failures stay in
the PR that finds them. The [current plan](PLAN.md) identifies the remaining
storage implementation and qualification work. Passing focused tests does not
qualify an unfinished implementation for merging. Qualification includes compiled
regressions, delayed-publication recovery, control-request fairness and complete
workflows on the final head.

For an entry, include a reproducer, affected boundary, observed behavior and
the specific blocker. Remove repaired entries; preserve provenance in their
implementing changes instead of keeping a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
