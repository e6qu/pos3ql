# Blocked defects

This file records only defects whose repair is prevented by an external blocker
or genuine intractability. Fixable defects must be fixed in the change that
finds them. Planned work and architecture limits belong in [PLAN.md](PLAN.md).

The saved-file scan review on 2026-10-14 checked delayed row selection through
table replacement and Storage shutdown, snapshot filtering, nested callbacks,
cancellation, file release, and bounded admission failures. These scans retain
their own schema, files, and reader without consulting live table state.
No known defects meet this file's inclusion criteria. This does not mean all
planned work is finished or that every possible bug has been ruled out.
Resolved investigations are indexed in [history](docs/history/README.md).

For a new entry, include a reproducer, affected boundary, observed behavior,
and the specific blocker that prevents a fix. Remove it when repaired; preserve
provenance in the implementing change rather than adding a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
