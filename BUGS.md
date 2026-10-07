# Blocked defects

This file records only defects whose repair is prevented by an external blocker
or genuine intractability. Fixable defects must be fixed in the change that
finds them. Planned work and architecture limits belong in [PLAN.md](PLAN.md).

There are currently no defects that meet these inclusion criteria.

Live table and transaction-visible definition references now retain their
relation's read guard. Identity, rows, statistics, and maintenance use one slot
boundary; exclusive lifecycle mutation cannot coexist with retained references.
The lock storage is charged at startup. Shared table mutation and engine
publication remain roadmap work, not deferred defects.

SQL and wire engine fixtures now charge retained reader capacity through one
constructor,
preserving prior workspace headroom for every configured table/workspace size.

DML definition readers now retain immutable startup-budgeted images across
mutable callbacks, rollback, publication, and table identity reuse. Per-row
UPDATE and MERGE readers reuse those images. Exhaustion and stale identity
reacquisition fail explicitly; error returns release the retained capacity.
COPY and DDL pre-change readers use the same pool. Nested-trigger exhaustion
rolls back the outer mutation and frees capacity for retry. Dense pool occupancy avoids touching every reserved wide image at startup.
The wide mutable-replay/cursor fixture now has a dedicated optimized gate;
unoptimized runs at both merged main and the reader head exceeded an isolated
six-minute deadline. Optimized runs passed within the unchanged CI ceiling.
These changes introduce no deferred defect. Clippy also exposed deprecated atomic update
calls; the catalog clock now reuses its bounded compare-and-exchange boundary
and TLS accounting retains its saturating compare-and-exchange behavior.

Table-owned serial advances previously used unchecked signed addition. The
counter boundary now checks all integer widths before mutation. Dirty-state
acknowledgement is tied to the transaction's unchanged staged positions so a
later advance or reset cannot be lost. These repairs introduce no deferred bug.
Serial WAL recovery now rejects missing relations and out-of-definition columns
instead of accepting a record without restoring its position.

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
