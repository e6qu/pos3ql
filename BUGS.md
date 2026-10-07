# Blocked defects

This file records only defects whose repair is prevented by an external blocker
or genuine intractability. Fixable defects must be fixed in the change that
finds them. Planned work and architecture limits belong in [PLAN.md](PLAN.md).

There are currently no defects that meet these inclusion criteria.

Pending and committed row-version arrays now share a guarded owner with their
free lists. Allocator calls cannot pair an array with another pool's free list.
Full chain reads retain one view; rollback, pruning, slot reuse,
and heap relocation require exclusive ownership. Fixed capacities and named
exhaustion remain unchanged, with exact startup charging for pool controls.
The combined row-map/version lifecycle remains roadmap work; this change
introduces no deferred defect. The outer differential harness now also streams
regression progress instead of hiding it until completion. Runner stack samples
identified wide routine copies during per-row type resolution. Lookup now
filters transaction-visible metadata before copying candidate payloads, with
no routine guard held across nested catalog resolution.

Row maps now return detached row-state images from shared lookups and guarded
iteration. Recovery, rollback, cloning, and slot reuse use the same map access
boundary. Locks are charged at startup; capacity failure preserves retained
rows. Query-scope definition ownership and shared row publication remain
explicit roadmap work. This change introduces no deferred defect.
All-target lint also corrected catalog test loops and an obsolete conversion;
worker transaction and catalog identities are preserved.
The first forced-spill slice and an instrumented reference corpus slice
exhausted their fixed CI deadlines. Each now has two complementary slices;
the timeout guard proves all current and future corpus ordinals still have
exactly one owner in both matrices. No test or limit was
removed or reduced. The ordinary PostgreSQL regression
file slice also has complementary workers; upstream ranges stay together and
statement progress is streamed so deadline failures retain diagnostics.

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
