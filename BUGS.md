# Blocked defects

This file records defects whose repair is prevented by an external blocker
or genuine intractability. Fixable defects must be fixed in the change that
finds them. Planned work and architecture limits belong in [PLAN.md](PLAN.md).

No defects currently require an external unblock. Database-local type moves,
cast planning and SQL-function sequence execution are repaired in this change.
Remaining implementation work and concurrency limits belong in PLAN.md.

For a new entry, include a reproducer, affected boundary, observed behavior,
and the specific blocker that prevents a fix. Remove it when repaired; preserve
provenance in the implementing change rather than adding a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
