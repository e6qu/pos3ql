# PostgreSQL function results in FROM

[reference.zip](reference.zip), SHA-256
`933b246ff62e34128462b3b6c9f85f44fb9d0a4b53fd92861e7040baa3127e6e`,
retains PostgreSQL 18.6 results for fixture 244 on macOS arm64. Extract it and run
`python3 verify-reference.py .` to check file hashes, all 17 result rows, absence
of errors and the reference server's resource settings.

`SELECT n,label FROM from_procedure(7)` exposes two named output columns in one
row. The reference also covers SQL and PL/pgSQL calls, aliases, ordinality,
`ROWS FROM`, lateral inputs, single output names, sets, empty scalar queries,
null results and named composite types. A strict function with a null input
still evaluates its other input in `FROM`; its sequence advances once.

SQL, runner script, commands, raw output, server logs, binary hashes and source
provenance are archived. The guarded probe used a Unix socket, 16 MiB shared
buffers, 1 MiB work memory and five connection slots. This verifies PostgreSQL
semantics; pos3ql runtime and allocation checks run separately on CI. These
results make no performance or production memory claim.
