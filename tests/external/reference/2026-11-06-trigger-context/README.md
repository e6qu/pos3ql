# PostgreSQL trigger context types

[reference.zip](reference.zip), SHA-256
`d9be051156f3bc8a3f7fbfb0e6f50a7a5b5aa923ca375eaf5bc083244eac8ec0`,
retains two PostgreSQL 18.6 results for fixture 254 on macOS arm64. Extract it
and run `python3 verify-reference.py .` to verify hashes, results, zero errors
and bounded reference-server settings.

`TG_NAME`, `TG_RELNAME`, `TG_TABLE_NAME` and `TG_TABLE_SCHEMA` have type `name`;
`TG_RELID` is `oid`, `TG_NARGS` is `integer`, and `TG_ARGV` is `text[]`.
`TG_WHEN`, `TG_LEVEL` and `TG_OP` are text. With no trigger arguments,
`TG_ARGV` is NULL; with arguments its first item has index zero.
`TG_TABLESPACE` is absent and reading it raises `42703`, rather than returning
an invented NULL value.

The archive includes SQL, runner, commands, raw output, logs, binary hashes
and provenance. The guarded Unix-socket probe used 16 MiB shared buffers,
1 MiB work memory and five connections. pos3ql runtime and allocation checks
run separately on CI; these observations make no performance or production
memory claim.
