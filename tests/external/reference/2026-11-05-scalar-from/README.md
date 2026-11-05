# PostgreSQL function results in FROM

[reference.zip](reference.zip), SHA-256
`e4443e4d405d5efe4997081394ad5263f137bd97e7c470dd003b491a2f5390ee`,
retains PostgreSQL 18.6 results for fixture 244 on macOS arm64. Extract it and run
`python3 verify-reference.py .` to check file hashes, all 23 result rows, absence
of errors and the reference server's resource settings.

`SELECT n,label FROM from_procedure(7)` exposes two named output columns in one
row. The reference also covers SQL and PL/pgSQL calls, aliases, ordinality,
`ROWS FROM`, lateral inputs, single output names, sets, empty scalar queries,
null results, whole and nested named rows, and empty named-row queries. A field
constraint calling a helper advances its sequence exactly once during result
conversion. A strict function with a null input still evaluates its other input
in `FROM`; that sequence also advances once.

SQL, runner script, commands, raw output, server logs, binary hashes and source
provenance are archived. The guarded probe used a Unix socket, 16 MiB shared
buffers, 1 MiB work memory and five connection slots. This verifies PostgreSQL
semantics; pos3ql runtime and allocation checks run separately on CI. These
results make no performance or production memory claim.
