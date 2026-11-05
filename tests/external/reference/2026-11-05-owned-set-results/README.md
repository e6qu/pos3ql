# PostgreSQL set-function results

[reference.zip](reference.zip), SHA-256
`13913f479bacb1b4a4c388662479ff0a6b6f760ce957eeb540a354e4d2e2f3bd`,
retains eight PostgreSQL 18.6 results for fixture 248 on macOS arm64. Extract it
and run `python3 verify-reference.py .` to verify hashes, results, zero errors
and bounded reference-server settings.

A function returns 4,096 text values; another catches an error after returning
part of a query and keeps those earlier rows. Empty results remain empty. An
integer loop counter that shadows a text output variable does not replace the
output variable's value in `RETURN NEXT`.

SQL, runner, commands, raw output, logs, binary hashes and source provenance are
archived. The guarded Unix-socket probe used 16 MiB shared buffers, 1 MiB work
memory and five connections. pos3ql runtime and allocation checks run separately
on CI. These observations make no performance or production memory claim.
