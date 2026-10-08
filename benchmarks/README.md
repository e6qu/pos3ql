# Retained benchmark evidence

These dated runs preserve their original workload, revision/binary hashes,
service/storage provenance, raw JSON, and derived reports. They are exploratory
measurements on the stated hosts, not current production guarantees. Use
[performance.md](../docs/performance.md) to run the harness and
[PLAN.md](../PLAN.md) for outstanding representative qualification.

The resource-matched matrix retains both stock PostgreSQL 18 controls for each
fixture, MinIO, and SeaweedFS leg. PostgreSQL uses its ordinary local durable
tier. MinIO and SeaweedFS here are same-host services, not independently operated
storage. Different operation counts and generation shapes limit timing ratios;
request metrics describe each engine's actual persistence design.

## Comparison evidence

| Date | Evidence |
|---|---|
| 2026-09-20 | [Exploratory PostgreSQL 18 baseline, 2026-09-20](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-20-postgresql18-local-apfs/README.md) |
| 2026-09-21 | [Paired checkpoint overlap with PostgreSQL 18, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-21-postgresql18-checkpoint-1000/README.md) |
| 2026-09-23 | [10,000-row object-store matrix](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-23-object-store-matrix-10000/README.md) |
| 2026-09-23 | [10,000-row resource-matched PostgreSQL matrix](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-23-resource-matched-postgresql-10000/README.md) |

Follow each run's README for qualifications and raw artifact locations. Preserve
these files during documentation cleanup; do not regenerate a report against
new code under an old date. Historical optimization notes are indexed in
[implementation history](../docs/history/README.md).

The [complete historical catalog](https://github.com/e6qu/pos3ql/blob/51a21b6b7393022b92c4f6f86c889e9fa3394b93/benchmarks/README.md)
indexes the focused checkpoint experiments. Their original directories and raw
artifacts remain unchanged; they document past implementation costs.
