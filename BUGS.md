# Blocked defects

Only defects blocked by an external condition or a genuinely intractable repair
belong here. Fix every other defect, including CI regressions, in the PR that
finds it. Keep repaired defects and their evidence in the implementing PR.
Planned work and acceptance checks belong in [PLAN.md](PLAN.md).

No defects currently meet this boundary. Repair reproducible admission,
publication, cancellation and recovery failures in the change that finds them.
Catalog visibility repairs must preserve both explicit catalog-query snapshots
and current object resolution; neither boundary is a reason to defer the other.
Permission row identity, rollback and durable replay are part of that repair.
Comment row identity, lookup versus listing behavior, and rejection of malformed
stored keys belong to the same boundary.
Index hints and relation counts must survive recovery without turning private
column estimates into committed data or reusing a replacement table's identity.

Implemented guarantees belong in the [current behavior guide](docs/implemented-baseline.md)
and [storage format guide](docs/durable-format.md). Keep regression evidence in
the implementing PR.

Each entry must include a reproducer, affected boundary, observed behavior and
exact blocker. Publication, rollback and recovery defects follow the same rule.
Fault tests must restart a terminally failed instance before checking recovery;
they must reject partial transactions.
Remove repaired entries; preserve their evidence in the implementing
change instead of keeping a resolved-work journal.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
