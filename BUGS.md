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
Record an entry here only with its reproducer, observed behavior and exact
external blocker or reason it cannot be repaired. Keep resolved investigations
with their fixing code, tests and retained reference evidence.

Pending CI, uncollected measurements and reproducible failures being repaired
belong to the active PR and [PLAN.md](PLAN.md). They are not blocked defects.
[CONTRIBUTING.md](CONTRIBUTING.md) owns verification and failure diagnostics;
current behavior and limits belong in the [owning guides](docs/README.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
