# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix every reproducible defect in the PR that finds it. Record an entry only
with its reproducer, observed behavior and exact external blocker or reason it
cannot be repaired. Keep reproductions, regression checks and PostgreSQL
comparisons in the fixing PR, including declared result types, input lifetimes,
argument and sequence effects, and when errors occur.
Include overload selection with typed nulls and the declared inputs of correlated
table functions; a correct non-null value alone does not establish its SQL type.
Compare resident and forced-spill paths with the same SQL, including nested
outer references and single-column table-function aliases.
For range predicates, compare null truth tables, repeated sequence/subquery
calls, operator binding and errors reached only by later fetches.
Compare three-part catalog column references during planning and execution,
including prepared queries, nested queries, wrong schemas and aliases.
Keep actual client-generated SQL and migration failures as regressions; preserve
psycopg2, SQLAlchemy and Alembic transcripts against the PostgreSQL reference.
Check catalog-character comparisons with text arrays and schema-qualified outer
references, including their types, collations and alias errors.
Memory comparisons must separate configured reservations, drawn startup budget
and measured process memory; retain both transaction modes and the revision.
The live built-in inventory must reject wrong properties for registered routines;
missing catalog entries remain an explicit completion gap in PLAN.md.
Preserve failed outputs and diagnostics before removing temporary fixtures.
Check native Linux amd64, Linux arm64 and macOS arm64 builds when changing C
interfaces; a passing build on one target does not establish another's ABI.
See [CONTRIBUTING.md](CONTRIBUTING.md).

Unfinished features and their verification criteria belong in [PLAN.md](PLAN.md).
Current guarantees belong in the [behavior guide](docs/implemented-baseline.md)
and its [owning technical guides](docs/README.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
