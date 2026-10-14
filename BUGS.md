# Blocked defects

This file records only defects whose repair is prevented by an external blocker
or genuine intractability. Fixable defects must be fixed in the change that
finds them. Planned work and architecture limits belong in [PLAN.md](PLAN.md).

The deferred executor-read review on 2026-10-14 covers enum rewrite sources and
MERGE target bytes, pending and saved-file rows, arrays, partition moves,
savepoint rollback, arena exhaustion, and recovery. Remaining reader-lifetime
work is tracked in PLAN.md.
No known defects meet this file's inclusion criteria. This does not mean all
planned work is finished or that every possible bug has been ruled out.
Resolved investigations are indexed in [history](docs/history/README.md).

For a new entry, include a reproducer, affected boundary, observed behavior,
and the specific blocker that prevents a fix. Remove it when repaired; preserve
provenance in the implementing change rather than adding a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
