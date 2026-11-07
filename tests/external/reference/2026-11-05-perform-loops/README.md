# PostgreSQL discarded query results in loops

[reference.zip](reference.zip), SHA-256
`0445e170aafbe54fbd74fe3f0c6f6262534ef21016784dedca5bf3db7f6e7e3d`,
retains PostgreSQL 18.6 results for fixture 245 on macOS arm64. Extract it and run
`python3 verify-reference.py .` to check every file hash, result and bounded
reference-server settings.

The block counts `FOUND` and `ROW_COUNT` across 4,096 iterations, calls a nested
text-returning function on every iteration, and runs `nextval` every 128.
PostgreSQL returns `2048|2048|32`: half the queries find a row, and the sequence
advances exactly 32 times. Discarding function output must not repeat effects.

The archive includes SQL, runner script, commands, raw output, server logs,
binary hashes and provenance. The guarded probe used a Unix socket, 16 MiB
shared buffers, 1 MiB work memory and five connection slots. This establishes
reference semantics. The pos3ql allocation and statement-memory regression
runs separately on CI in both transaction modes at its existing capacity.
