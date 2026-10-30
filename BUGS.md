# Blocked defects

No known defects are blocked by an external dependency or an intractable problem.

Fix every reproducible defect in the PR that finds it. Record an entry only
with its reproducer, observed behavior and exact external blocker or reason it
cannot be repaired. Keep repaired-defect evidence, ownership checks and PostgreSQL comparisons in
the fixing PR, including exhausted-memory behavior before irreversible effects
and retained readers surviving command-arena resets and owner moves. Keep
active-relation checks, failed-fetch cleanup, wide-row projections and storage-wait retries in the same
regression coverage.
Failed PostgreSQL comparisons preserve their outputs and diagnostics before
temporary files are removed.
See [CONTRIBUTING.md](CONTRIBUTING.md).

Unfinished features and their verification criteria belong in [PLAN.md](PLAN.md).
Current guarantees belong in the [behavior guide](docs/implemented-baseline.md)
and its [owning technical guides](docs/README.md).

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
