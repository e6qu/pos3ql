# Retained benchmark evidence

Each dated run keeps its original workload, revision and binary hashes,
storage provenance, raw JSON and report. They are exploratory measurements on
the stated hosts, not current performance claims; new code needs a new run.
[performance.md](../docs/performance.md#what-the-retained-measurements-actually-show)
explains how to run the suite and how to read these results.

**User example:** before comparing an old result with a new run, match the
commit, row count, clients, hardware and storage. A faster number from a
different setup does not show that the database became faster.

## Comparison evidence

| Date | Evidence |
|---|---|
| 2026-09-20 | [Exploratory PostgreSQL 18 baseline, 2026-09-20](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-20-postgresql18-local-apfs/README.md) |
| 2026-09-21 | [Paired checkpoint overlap with PostgreSQL 18, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-21-postgresql18-checkpoint-1000/README.md) |
| 2026-09-23 | [10,000-row object-store matrix](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-23-object-store-matrix-10000/README.md) |
| 2026-09-23 | [10,000-row resource-matched PostgreSQL matrix](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-23-resource-matched-postgresql-10000/README.md) |
| 2026-10-30 | [Empty embedded engine startup budget and resident memory](https://github.com/e6qu/pos3ql/blob/6ff1ac865a86a5318cb4934fbf01be0926667811/benchmarks/baselines/2026-10-30-embedded-startup/README.md) |
| 2026-10-30 | [Named empty-engine startup reservations](https://github.com/e6qu/pos3ql/blob/6ff1ac865a86a5318cb4934fbf01be0926667811/benchmarks/baselines/2026-10-30-embedded-components/README.md) |
| 2026-10-30 | [Empty-engine memory before and after lazy definition initialization](https://github.com/e6qu/pos3ql/blob/4054a6d3c79e916728f8e84d84139eae6d86b501/benchmarks/baselines/2026-10-30-embedded-lazy-definitions/README.md) |
| 2026-10-31 | [Instrumented function lookup repair comparison](https://github.com/e6qu/pos3ql/blob/cfd803e74af661d4afc9d9e366b9293d08add65c/benchmarks/baselines/2026-10-31-routine-lookup/README.md) |

Each run's README states its qualifications and raw artifact locations. Never
regenerate a report against new code under an old date. The
[complete historical catalog](https://github.com/e6qu/pos3ql/blob/51a21b6b7393022b92c4f6f86c889e9fa3394b93/benchmarks/README.md)
also indexes the focused checkpoint experiments.
