# Blocked defects

This file records defects whose repair is prevented by an external blocker
or genuine intractability. Fixable defects must be fixed in the change that
finds them. Planned work and architecture limits belong in [PLAN.md](PLAN.md).

No externally blocked defects are currently recorded. Remaining work, including
complete type publication, bounded heap remapping and other cleanup walks,
checkpoint coordination, object deletion and execution workers, belongs in PLAN.md.
Completed contracts, accepted storage formats and explicit type limits belong
in their owning technical guides. Startup checks checkpoint and commit record
formats, writer identities and allocator counters before changing writer
ownership. Statement and transaction completion schedule history cleanup;
bounded passes must preserve active readers, skipped work and later releases.
Heap capture must keep its finite cursor and release owners between turns;
new references, including database clones, must respect its append boundary.
Unsupported older pos3ql versions are not defects.

For a new entry, include a reproducer, affected boundary, observed behavior,
and the specific blocker that prevents a fix. Remove it when repaired; preserve
provenance in the implementing change rather than adding a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
