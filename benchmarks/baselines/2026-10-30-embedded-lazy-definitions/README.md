# Empty-engine memory after lazy definition initialization

Leaving unused pending table-definition payloads uninitialized reduces measured
startup memory. Each sample starts a separate empty engine process, with no
application tables or client sessions. These are debug builds on Linux x86_64;
configured table slots are capacity, not populated tables.

| Transaction mode | Configured tables | Before peak RSS, MiB | After peak RSS, MiB | Reduction |
|---|---:|---:|---:|---:|
| MVCC | 1 | 314.89 | 295.46 | 6.2% |
| MVCC | 8 | 409.99 | 326.96 | 20.3% |
| MVCC | 32 | 734.49 | 431.21 | 41.3% |
| Locking | 1 | 315.01 | 295.27 | 6.3% |
| Locking | 8 | 409.82 | 326.64 | 20.3% |
| Locking | 32 | 734.75 | 431.37 | 41.3% |

RSS is resident process memory: pages actually held in RAM. MiB means 1,048,576
bytes. The fully reserved startup budget changes by only 48 to 1,040 bytes
between these revisions. The improvement comes from avoiding initialization of
unused wide payloads, not from removing the capacity promised at startup.

Both runs use one connection, one query workspace, 8,192 rows per table slot,
8,192 large-object pages, a 1 MiB memtable, an 8 MiB work arena and a 128 MiB
virtual startup stack. MVCC retains older row values and reserves 65,536 shared
history slots and 1 MiB of catalog history; locking mode reserves neither.
The per-row version limit is eight in both modes. Other helper settings are
unchanged. Every explicit setting is recorded in the raw samples.

## Provenance and reproduction

Before: revision `dfbdc1c193ec18d749a17d13750935cb1b699fa6`,
[job](https://github.com/e6qu/pos3ql/actions/runs/41147047515/job/124374470631),
[artifact](https://github.com/e6qu/pos3ql/actions/runs/41147047515/artifacts/13022011155).
Its measurements completed despite a separate prepared-history regression
failure in that job. The after revision also fixes that regression; this is a
comparison of the recorded revisions, not an isolated single-change experiment.

After: revision `6ff1ac865a86a5318cb4934fbf01be0926667811`,
[job](https://github.com/e6qu/pos3ql/actions/runs/41149538268/job/124382493520),
[artifact](https://github.com/e6qu/pos3ql/actions/runs/41149538268/artifacts/13023355769).
The focused regression tests and measurements both passed.

The [before samples](before/samples.jsonl), [after samples](after/samples.jsonl),
compiler identities and revision files are retained with their named startup
budget logs. [SHA256SUMS](SHA256SUMS) checks all raw evidence. In every sample,
the sum of the named budget draws equals both planned and drawn engine bytes.
Disposable database directories are excluded.

At either recorded revision, build `cargo build --example embedded-memory --locked`
and run the helper once per process, using a fresh data directory:

```sh
target/debug/examples/embedded-memory --tables 32 --table-rows 8192 \
  --mode mvcc --row-history-slots 65536 --data-dir /tmp/pos3ql-memory-mvcc-32 \
  --budget-limit-bytes 2147483648
```

Repeat with table capacities 1, 8 and 32; for locking use `--mode locking`
and `--row-history-slots 0`. Compiler versions are in the corresponding
`compiler.txt` files. These single samples do not establish measurement variance,
a production minimum, populated-table costs or client-session memory. Substantial
fixed reservations and per-table metadata remain; see the
[earlier component breakdown](../2026-10-30-embedded-components/README.md).
