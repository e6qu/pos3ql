# Named empty-engine startup reservations, 2026-10-30 UTC

This run adds named allocation-budget records to the
[earlier empty-engine measurement](../2026-10-30-embedded-startup/README.md).
It uses the same debug helper settings and six samples: MVCC and locking,
with 1, 8 or 32 configured table slots. Each sample starts a separate process,
without client sessions or application tables.

Source: [CI job](https://github.com/e6qu/pos3ql/actions/runs/41145264531/job/124368920057),
[artifact](https://github.com/e6qu/pos3ql/actions/runs/41145264531/artifacts/13020647636).
The job's SQL regressions failed in its simulated object-store fixture; the
separate startup measurements completed. Keep those outcomes distinct.
The [revision](revision.txt), [compiler](compiler.txt), [raw samples](samples.jsonl)
and six `*.budget.log` files preserve the evidence. [SHA256SUMS](SHA256SUMS)
checks every raw file.

Each `budget-draw` record contains bytes and the component's name, separated
by tabs. Repeated names are summed. Their sum equals the sample's drawn engine
budget for every sample. These are reserved byte counts, not resident pages
or a measurement of which component touched a page.

| Component, MVCC mode | 1 configured table, MiB | 32 configured tables, MiB |
|---|---:|---:|
| Retained table-definition images | 18.74 | 309.27 |
| Pending table-definition slots | 18.36 | 302.95 |
| Composite-definition image pool | 204.01 | 204.01 |
| Stored query dependencies | 120.09 | 120.09 |
| Committed row-history slots | 7.00 | 115.50 |

MiB means 1,048,576 bytes. The largest fixed and per-table reservations are
not all row data. Avoiding unnecessary definition copies and unused slot
initialization therefore needs measurement alongside shared row-history sizing.
Reducing an untouched reservation alone may barely change resident memory.
No populated-table, session, release-build or production performance claim is
established by this run.
