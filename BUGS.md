# Known bugs

This file records only defects that cannot be fixed in the current change
because an external blocker or genuine intractability prevents a fix. Fixable
defects belong in the implementation that discovers them; planned engineering
work and architecture limits belong in [PLAN.md](PLAN.md).

There are currently no defects that meet this file's inclusion criteria.
The geometric, ranked nearest-neighbor, inverted posting, signature, and
interval index-navigation,
checkpoint-maintenance capacity and publication handoff, concurrent-harness
port, integration-scratch, accepted-definition list, and statement-width
audits have no external blocker; their fixes are qualified in the
implementation and regression suites. The catalog-version pool audit likewise
found no external blocker: the per-object table-definition, statistics, and
routine-dependency ceiling is startup-configurable and covered by savepoint,
exact-memory, and cold-recovery regressions.
The row-version and spill-generation audit also found no external blocker:
both former eight-entry ceilings are startup-configurable, fully accounted,
and covered across exhaustion, reuse, checkpoint publication, and empty-cache
recovery.
The extended-statistics and BRIN-maintenance capacity audit found no external
blocker: the former table-derived ceiling and per-index inline array are
replaced by configured startup pools and qualified with maximum trigger
catalogs through checkpoint retry and object-cold recovery.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
