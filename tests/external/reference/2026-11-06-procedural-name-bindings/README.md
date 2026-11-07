# PostgreSQL procedural names and variable bindings

[reference.zip](reference.zip), SHA-256
`da383096464140ddc26acb04029dfef2bd028eb09bd7daccf158b9aee5bf080d`,
retains four PostgreSQL 18.6 results for fixture 255 on macOS arm64. Extract it
and run `python3 verify-reference.py .` to verify file hashes, results, zero
errors and the reference server's memory and timeout settings.

Cursor variable names can contain quoted spaces and doubled quotes in `OPEN`,
`FETCH`, `MOVE` and `CLOSE`. A declared integer `FOUND` keeps its integer value
and type. A local `TG_NAME` shadows the trigger context variable. A handler's
implicit `SQLSTATE` and `SQLERRM` shadow outer integer declarations, including
when a cursor captures them. A trigger conflict update sees the handler's text
SQLSTATE and the declared integer FOUND. Quoted `"NEW"` is distinct from `new`;
reading `"NEW".id` raises `42P01`.

The archive contains SQL, runner, commands, raw results, logs, binary hashes
and provenance. The guarded Unix-socket probe used 16 MiB shared buffers,
1 MiB work memory and five connections. pos3ql execution and allocation checks
run separately on CI. These observations make no performance claim.
