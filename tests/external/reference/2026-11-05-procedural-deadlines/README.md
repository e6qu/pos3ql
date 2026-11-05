# PostgreSQL procedural and protocol deadlines

[reference.zip](reference.zip), SHA-256
`8c644fc42099f1d547e6b58a7705618ae5425ad169e782c7d43ea29003fe5217`,
records PostgreSQL 18.6 deadline behavior on macOS arm64. The archive retains
exact SQL and wire messages, scripts, server commands, binary hashes, raw
results and provenance. `SHA256SUMS` checks every other archived file.

An explicit `query_canceled` handler can continue after a timeout. `WHEN OTHERS`
does not catch it, and a loop containing only `NULL` still times out. The SQL
probe returns `caught`, `after`, `0` and two deliberate `57014` errors.

The wire probe waits 400 ms after Parse, Bind or Describe with an explicit
100 ms timeout. PostgreSQL sends no error while waiting for another message;
continued execution returns `57014` with no rows. Sync restores a usable session.
After the same wait, empty COPY completion succeeds; supplying a data row before
completion returns `57014`. This does not measure production throughput or
qualify pos3ql. Its native, allocation, Miri and full final-head checks remain
required.

The small reference server used only a Unix socket, 16 MiB shared buffers,
1 MiB work memory and five connection slots under the local resource guard.
PostgreSQL documents the timer's command and protocol boundaries in
[client settings](https://www.postgresql.org/docs/18/runtime-config-client.html).
