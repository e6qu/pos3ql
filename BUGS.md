# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix reproducible defects in the PR that finds them, including incidental defects
and failures of completion output, retained writes, instruction effects or their
original undo boundaries, declared record types after cursor commit, and quoted
SQL boundaries or completed function results lost with temporary query memory.
This includes comparison fixtures and CI checks that prevent tests from running.
Preserve PostgreSQL warnings and record-field types and collations as well as
successful row values; expected diagnostics need exact comparison coverage.
Include empty-query descriptions, saved local-record fields and the saved
advisory-call position across a concurrent index command's internal commits.
Two record fields with a text domain must remain comparable to each other.
Metadata discovery must preserve whole-row alias resolution in procedural SQL.
Selecting a column by name must preserve its declared domain type through
derived tables as well as direct table reads and wildcard expansion.
Expanded records must occupy their actual output width in ordering, grouping,
window queries, set operations and deferred evaluation after sorting.
Derived-table descriptions must resolve their own WITH names before column
lookup, including CTEs that shadow a trigger's transition table. An empty outer
WITH clause cannot bypass nested bindings. Checked-input restoration must bind
the executable initializer as well as its discovery tree. SELECT INTO descriptions must retain
local record-field types; ROW field positions count expanded fields. Multi-column
assignment subqueries participate in the same binding and dependency walk and
retain one shared source rather than repeating its effects for each field.
INTO target parsing must preserve following set operations and quoted commas.
Runtime ROW field identities and wire descriptions use expanded metadata rather
than indexing the unexpanded constructor arguments.
Type-only query contexts must forward complete whole-row metadata, including
subquery collation analysis and postponed output after sorting.
RETURNING, ordinary write expressions and conflict-update expressions share
the same reference-kind walk, with explicit old/new and excluded namespaces.
Sequence-only SQL helpers called inside trigger source queries must use the
active statement session and preserve assignment order and once-only effects.
Platform lock checks must exercise the actual native API with race detection;
preserve direct-API controls and source provenance for foreign-emulation errors.
Dependency capture must resolve whole rows, selected fields and schema-qualified
columns while retaining the columns actually required by later schema changes.
Fixed function-timing capacity must fail explicitly and return entries after
early errors or unwinding; silently disabling requested tracking is a defect.
Write completion must reuse a captured table's identical row schema instead of
reserving a second image. A paused firing pins that same cell while completion
retains it; independent pin release cannot recycle another owner's schema.
Capacity errors must roll back and permit repeated
nested-trigger retries without increasing the configured image pool.
Record an entry here only with its reproducer, observed behavior and exact
external blocker or reason it cannot be repaired. Keep resolved investigations
with their fixing code, tests and retained reference evidence.

Pending CI, uncollected measurements and reproducible failures being repaired
belong to the active PR and [PLAN.md](PLAN.md). They are not blocked defects.
[CONTRIBUTING.md](CONTRIBUTING.md) owns verification and failure diagnostics;
current behavior and limits belong in the [owning guides](docs/README.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
