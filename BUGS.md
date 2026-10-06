# Blocked defects

This file records only defects whose repair is prevented by an external blocker
or genuine intractability. Fixable defects must be fixed in the change that
finds them. Planned work and architecture limits belong in [PLAN.md](PLAN.md).

There are currently no defects that meet these inclusion criteria.

Table-owned serial advances previously used unchecked signed addition. The
counter boundary now checks all integer widths before mutation. Dirty-state
acknowledgement is tied to the transaction's unchanged staged positions so a
later advance or reset cannot be lost. These repairs introduce no deferred bug.

The 2026-10-06 documentation review corrected stale width and dependency claims,
obsolete plan references, duplicated progress histories, and ambiguous status
and qualification language. Failed replacement guidance now requires validated
writer ownership. Release documentation preserves its relative link layout and
CI checks source and packaged destinations. These corrections introduce no
deferred bug. The no-op guard's obsolete debt-exemption guidance was removed;
its accepted source remains unchanged from the former zero-debt budget.
Resolved investigations remain available in the
[historical record](docs/history/README.md).

For any new entry, include a reproducer, affected boundary, observed behavior,
and the specific blocker that prevents a fix. Remove it when repaired; preserve
provenance in the implementing change.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
