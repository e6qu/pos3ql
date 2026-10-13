# Blocked defects

This file records only defects whose repair is prevented by an external blocker
or genuine intractability. Fixable defects must be fixed in the change that
finds them. Planned work and architecture limits belong in [PLAN.md](PLAN.md).

The replication snapshot review on 2026-10-13 checked history retention,
replacement, concurrent import/expiration, connection and transaction identity,
capacity failures, database boundaries, and PostgreSQL command lifetimes.
The retention and import defects found during review are fixed in this change.
No known defects meet this file's inclusion criteria. This does not mean all
planned work is finished or that every possible bug has been ruled out.
Resolved investigations are indexed in [history](docs/history/README.md).

For a new entry, include a reproducer, affected boundary, observed behavior,
and the specific blocker that prevents a fix. Remove it when repaired; preserve
provenance in the implementing change rather than adding a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
