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

| Date | Evidence |
|---|---|
| 2026-09-20 | [Focused checkpoint block-reuse run, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-20-checkpoint-block-reuse-1000/README.md) |
| 2026-09-20 | [Focused checkpoint sort-run run, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-20-checkpoint-sort-runs-1000/README.md) |
| 2026-09-20 | [Focused checkpoint run, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-20-checkpoint-value-index-1000/README.md) |
| 2026-09-20 | [Exploratory PostgreSQL 18 baseline, 2026-09-20](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-20-postgresql18-local-apfs/README.md) |
| 2026-09-20 | [Exploratory PostgreSQL 18 comparison, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-20-postgresql18-local-apfs-1000/README.md) |
| 2026-09-21 | [Paced checkpoint deletion, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-21-checkpoint-deletion-pacing-1000/README.md) |
| 2026-09-21 | [Final-slice checkpoint publication, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-21-checkpoint-final-slice-1000/README.md) |
| 2026-09-21 | [Bounded full-roster checkpoint, 10,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-21-checkpoint-full-roster-bounded-10000/README.md) |
| 2026-09-21 | [Checkpoint phase profile, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-21-checkpoint-phase-1000/README.md) |
| 2026-09-21 | [Checkpoint reslice scale, 10,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-21-checkpoint-reslice-scale-10000/README.md) |
| 2026-09-21 | [Incremental checkpoint row reslicing, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-21-checkpoint-row-reslice-1000/README.md) |
| 2026-09-21 | [Checkpoint foreground correlation, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-21-checkpoint-stall-correlation-1000/README.md) |
| 2026-09-21 | [Selective value-index publication, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-21-checkpoint-value-dependencies-1000/README.md) |
| 2026-09-21 | [Paired checkpoint overlap with PostgreSQL 18, 1,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-21-postgresql18-checkpoint-1000/README.md) |
| 2026-09-22 | [Packed row deltas and bounded row merge reads, 10,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-22-checkpoint-row-format-10000/README.md) |
| 2026-09-22 | [Retained row-merge cursors and shared-container reads, 10,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-22-checkpoint-row-merge-containers-10000/README.md) |
| 2026-09-22 | [Immutable PAX group reuse during row merge, 10,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-22-checkpoint-row-merge-reuse-10000/README.md) |
| 2026-09-22 | [Paced value-index checkpoint writer, 10,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-22-checkpoint-value-index-pacing-10000/README.md) |
| 2026-09-22 | [Paced value-index source and sort, 10,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-22-checkpoint-value-index-schedule-10000/README.md) |
| 2026-09-23 | [Exact row-delta discovery, 10,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-23-checkpoint-row-delta-discovery-10000/README.md) |
| 2026-09-23 | [Incremental value-index checkpoint merge, 10,000 rows](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-23-checkpoint-value-index-delta-10000/README.md) |
| 2026-09-23 | [10,000-row object-store matrix](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-23-object-store-matrix-10000/README.md) |
| 2026-09-23 | [10,000-row resource-matched PostgreSQL matrix](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-23-resource-matched-postgresql-10000/README.md) |

Follow each run's README for qualifications and raw artifact locations. Preserve
these files during documentation cleanup; do not regenerate a report against
new code under an old date. Historical optimization notes are indexed in
[implementation history](../docs/history/README.md).
