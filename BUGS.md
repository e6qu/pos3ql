# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix reproducible defects in the PR that finds them, including incidental defects.
Record an entry here only with its reproducer, observed behavior and the exact
external blocker or reason it cannot be repaired. Ordinary regression cases
and resolved investigations belong in their fixing PR, tests and evidence.

Follow [CONTRIBUTING.md](CONTRIBUTING.md) for PostgreSQL comparisons, actual
application workflows, allocation checks, failure diagnostics and native platform
checks. Compare SQL values, declared types, errors and effects; preserve rollback,
reader ownership, checkpoint retention and recovery when changing shared memory.
Function checks include nested sequence effects, typed nulls and default inputs;
overload discovery must finish before running those inputs. Planning must respect
the registered volatility of built-ins, stored functions and user operators, so
maintenance and other effects cannot run as constant-folding probes.
Memory evidence distinguishes reservations from resident pages and records the
revision, compiler, settings and named component charges. Release checks validate
the documentation actually shipped in the archive.

Unfinished features and their acceptance criteria belong in [PLAN.md](PLAN.md).
Current guarantees belong in the [behavior guide](docs/implemented-baseline.md)
and its [owning technical guides](docs/README.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
