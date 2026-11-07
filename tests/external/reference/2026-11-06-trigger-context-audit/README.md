# PostgreSQL trigger context and nested conflict update

[reference.zip](reference.zip), SHA-256
`46333a765c988c5718418ba1c6c12fd0fc550885c64f82ea6ba24fc2e93d185d`,
retains the two PostgreSQL 18.6 results for fixture 256 on macOS arm64. Extract
it and run `python3 verify-reference.py .` to verify hashes, results, zero errors
and bounded reference-server settings.

A BEFORE trigger records its name, timing, level, operation, relation identity,
schema and arguments. `TG_NAME` has type `name` and `TG_RELID` has type `oid`.
An AFTER trigger uses its arguments in a nested conflict update. The native test
and live PostgreSQL comparison execute this same fixture. Dollar-quoted function
bodies keep SQL string literals independent of Rust string escaping.

The archive contains SQL, runner, commands, raw output, logs, binary hashes and
provenance. The guarded Unix-socket probe used 16 MiB shared buffers, 1 MiB work
memory and five connections. These observations make no performance claim.
