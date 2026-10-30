# Empty embedded engine startup, 2026-10-30 UTC

An empty engine can consume substantial memory before an application creates
its first table. These measurements record that cost before reducing it.
They measure configured table slots, not created tables or query throughput.

Source: [CI job](https://github.com/e6qu/pos3ql/actions/runs/41141571360/job/124356999120),
[original artifact](https://github.com/e6qu/pos3ql/actions/runs/41141571360/artifacts/13020095441).
The checked-in files preserve the artifact's measurement rows, compiler identity
and exact revision. The temporary empty database files are omitted.

The helper ran as an unoptimized debug executable on a GitHub-hosted Linux
amd64 runner. Each sample starts a separate process and one empty engine with
local durability. It sets one connection slot, one query workspace, a 1 MiB
row cache, 8 MiB work arena, 8,192 row positions per table and 8,192 large-object
page positions. Other settings come from
[that revision's development configuration](https://github.com/e6qu/pos3ql/blob/7330901ea4259b71e1fd843b58e91d1aeb984d1a/src/config.rs).
The helper reserves the same 128 MiB virtual worker stack as the server;
resident memory includes the stack pages actually touched. It creates no
client sessions and runs no SQL. Hardware details beyond the hosted runner
and recorded compiler target were not captured, so startup timings are
exploratory and should not be compared across machines.

| Transaction mode | Configured tables | Planned and drawn engine budget, MiB | Peak process resident memory, MiB |
|---|---:|---:|---:|
| MVCC | 1 | 637.26 | 314.85 |
| MVCC | 8 | 842.95 | 408.75 |
| MVCC | 32 | 1,533.07 | 730.12 |
| Locking | 1 | 626.26 | 314.00 |
| Locking | 8 | 796.95 | 408.74 |
| Locking | 32 | 1,367.07 | 730.08 |

MiB means 1,048,576 bytes. Planned and drawn budgets match in every sample;
they exclude the worker's virtual stack and are not process resident memory.
Increasing configured tables from 1 to 32 adds roughly 13.4 MiB of resident
memory per slot in this setup. Removing committed-row history in locking mode
reduces the reservation but barely changes the empty engine's resident memory.
This points to other startup structures as substantial costs; it does not
identify a single allocation as their cause. Later measurements must retain
named component reservations and use a new revision and evidence directory.

Reproduce the sample matrix on a dedicated runner, with a new directory for
each invocation:

```sh
cargo build --example embedded-memory --locked
target/debug/examples/embedded-memory --tables 1 --table-rows 8192 \
  --mode mvcc --data-dir /tmp/pos3ql-empty-mvcc-1 \
  --budget-limit-bytes 2147483648
```

Repeat with table counts 8 and 32, and with mode `locking`. The explicit 2 GiB
measurement ceiling rejects an oversized startup plan; it does not allocate
2 GiB. Larger catalogs, populated tables, sessions, concurrent queries and
object storage require separate measurements.

Raw file SHA-256 checksums are recorded in [SHA256SUMS](SHA256SUMS).
