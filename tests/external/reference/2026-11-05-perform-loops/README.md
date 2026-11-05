# PostgreSQL discarded query results in loops

[reference.zip](reference.zip), SHA-256
`3cd4de70dbbed5d560b23c96437c403138b77777defebbf370ea89d020e2ba93`,
retains PostgreSQL 18.6 results for fixture 245 on macOS arm64. Extract it and run
`python3 verify-reference.py .` to check every file hash, result and bounded
reference-server settings.

The block runs `PERFORM` 4,096 times, counts `FOUND` and `ROW_COUNT`, and calls
a nested text-returning function alongside `nextval` every 128 iterations.
PostgreSQL returns `2048|2048|32`: half the queries find a row, and the sequence
advances exactly 32 times. Discarding function output must not repeat effects.

The archive includes SQL, runner script, commands, raw output, server logs,
binary hashes and provenance. The guarded probe used a Unix socket, 16 MiB
shared buffers, 1 MiB work memory and five connection slots. This establishes
reference semantics. The pos3ql allocation and statement-memory regression
runs separately on CI in both transaction modes at its existing capacity.
