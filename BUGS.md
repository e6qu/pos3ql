# Blocked defects

Record only defects whose repair is prevented by an external blocker or genuine
intractability. Fix every fixable defect in the PR that finds it. Planned work
and incomplete architecture belong in [PLAN.md](PLAN.md).

No externally blocked defects are currently recorded. The current PR must
complete concurrent and batched storage reads and writes, physical layout,
and an explicit MVCC or locking mode before merging. Passing checks on its
earlier capture-only head do not qualify that expanded scope. Qualification
must cover the complete implementation on the final PR head, including delayed
prepared-transaction responses after their slots have been released and reused,
equal block payloads stored under different types, and failover rejection with
complete terminal error delivery. TLS error paths must also remain allocation-free
outside their isolated component. Its budget must be returned after failed
allocations, buffer shrinking and owned-error cleanup. COPY must retain its finishing
message while durability is pending, run triggers once, and preserve later
client messages through acknowledgement and empty-cache recovery. Internal wait statuses must not
reach SQL clients at BEGIN or concurrent partition-detach boundaries. Release-build checks
must retain the allocation calls rather than allowing optimization to remove them.

For an entry, include a reproducer, affected boundary, observed behavior and
the specific blocker. Remove repaired entries; preserve provenance in their
implementing changes instead of keeping a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
