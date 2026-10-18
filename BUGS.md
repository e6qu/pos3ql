# Blocked defects

This file records defects whose repair is prevented by an external blocker
or genuine intractability. Fixable defects must be fixed in the change that
finds them. Planned work and architecture limits belong in [PLAN.md](PLAN.md).

No externally blocked defects are currently recorded. Remaining work, including
complete type publication, heap capture and remapping, checkpoint coordination,
object deletion and execution workers, belongs in PLAN.md.
Completed contracts and explicit type limits belong in their owning technical guides.

For a new entry, include a reproducer, affected boundary, observed behavior,
and the specific blocker that prevents a fix. Remove it when repaired; preserve
provenance in the implementing change rather than adding a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
