# Blocked defects

This file records only defects whose repair is prevented by an external blocker
or genuine intractability. Fixable defects must be fixed in the change that
finds them. Planned work and architecture limits belong in [PLAN.md](PLAN.md).

The sorted-read review on 2026-10-14 checked pending, historical, and committed
byte retention through heap relocation, table replacement, and Storage shutdown.
Deferred sorted rows refer directly to their original source rather than
looking up reusable version chains again. Changed byte owners are rejected.
No known defects meet this file's inclusion criteria. This does not mean all
planned work is finished or that every possible bug has been ruled out.
Resolved investigations are indexed in [history](docs/history/README.md).

For a new entry, include a reproducer, affected boundary, observed behavior,
and the specific blocker that prevents a fix. Remove it when repaired; preserve
provenance in the implementing change rather than adding a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
