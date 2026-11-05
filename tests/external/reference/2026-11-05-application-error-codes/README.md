# PostgreSQL application error codes

[reference.zip](reference.zip), SHA-256
`2b919774f0b5874cf2d492d3d39c437012e700843225960e31c2ab837b3851f1`,
records PostgreSQL 18.6 on macOS arm64. Extract the archive and run
`python3 verify-application-errors.py .` to verify its ten file hashes, result
rows, errors and server configuration.

Applications can raise `PZ001`, `PZ002`, `PZ003`, `PZ004` and `PZ999` normally.
The reference catches each code through `OTHERS`, an exact handler and the
`PZ000` category handler, producing 15 audit rows. A function returning rows
catches `PZ004` and returns both `1` and `2`. Five uncaught statements return
their original error codes. No application error means an engine wait or an
early end to a result.

The archive preserves the SQL, runner, raw results, commands, binary hashes,
server logs and provenance. The resource guard ran the temporary server with
only a Unix socket, 16 MiB shared buffers, 1 MiB work memory and five connection
slots. This establishes reference semantics, not pos3ql execution or performance.
Native, allocation, Miri and full current-head qualification remain required.

PostgreSQL documents custom codes and category handlers in
[PL/pgSQL errors and messages](https://www.postgresql.org/docs/18/plpgsql-errors-and-messages.html).
