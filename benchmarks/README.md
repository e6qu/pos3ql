# Retained benchmark evidence

**User example:** to compare an old result with a new run, first match the
commit, row count, clients, hardware, and storage. A faster number from a
different setup does not establish that today's database became faster.
The reports below keep their original observations and caveats.

These dated runs preserve their original workload, revision/binary hashes,
service/storage provenance, raw JSON, and derived reports. They are exploratory
measurements on the stated hosts, not current production guarantees. Use
[performance.md](../docs/performance.md) to run the harness and
[PLAN.md](../PLAN.md) for outstanding representative qualification.

The matrix keeps two vanilla PostgreSQL 18 comparisons for each fixture,
MinIO, and SeaweedFS run: available host resources and the same CPU/memory
allowance as pos3ql. PostgreSQL saves data on ordinary local storage. MinIO and
SeaweedFS run on the same test host. Different operation counts and saved data
layouts limit timing comparisons; request metrics describe the actual storage
work performed.

The [current interpretation](../docs/performance.md#what-the-retained-measurements-actually-show)
quotes representative old results and distinguishes operations, statements and
transactions per second. New code requires a new run; theoretical block counts
and bandwidth calculations do not update these historical reports.

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

Follow each run's README for qualifications and raw artifact locations. Preserve
these files during documentation cleanup; do not regenerate a report against
new code under an old date. Historical optimization notes are indexed in
[implementation history](../docs/history/README.md).

The [complete historical catalog](https://github.com/e6qu/pos3ql/blob/51a21b6b7393022b92c4f6f86c889e9fa3394b93/benchmarks/README.md)
indexes the focused checkpoint experiments. Their original directories and raw
artifacts remain unchanged; they document past implementation costs.
