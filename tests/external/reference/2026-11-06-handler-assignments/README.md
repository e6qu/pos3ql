# PostgreSQL handler variables in JSON assignments

[reference.zip](reference.zip), SHA-256
`181e9a585fa17561bf40bf59554feac6e767d6998f3d717beb8cac0a42794e3a`,
retains two PostgreSQL 18.6 results for fixture 251 on macOS arm64. Extract it
and run `python3 verify-reference.py .` to verify hashes, results, zero errors
and bounded reference-server settings.

Inside an error handler, `document[SQLERRM] := to_jsonb(SQLSTATE)` uses the
caught message as a JSON key and its SQLSTATE as the value. Both a function's
local value and a BEFORE INSERT trigger's `NEW.document` retain `P0001` under
the key `caught`. Internal assignment rewriting must preserve these input
names even when it constructs a new expression.

The archive includes SQL, runner, commands, raw output, logs, binary hashes
and provenance. The guarded Unix-socket probe used 16 MiB shared buffers,
1 MiB work memory and five connections. pos3ql runtime and allocation checks
run separately on CI; these observations make no performance or production
memory claim.
