# PostgreSQL wide table-function results

[reference.zip](reference.zip), SHA-256
`5344169699713e117c9d5a1559699fc3b6cb35833b8dc2101c9514fc9a167637`,
retains two PostgreSQL 18.6 results for fixture 249 on macOS arm64. Extract it
and run `python3 verify-reference.py .` to verify hashes, results, zero errors
and bounded reference-server settings.

A function with no inputs returns 101 named integer columns. Selecting its
last field in the result list returns `100`; selecting its first and last
columns through `FROM` returns `0|100`. The output width is independent of the
100-input-argument limit.

SQL, runner, commands, raw output, logs, binary hashes and source provenance are
archived. The guarded Unix-socket probe used 16 MiB shared buffers, 1 MiB work
memory and five connections. pos3ql runtime and allocation checks run separately
on CI. These observations make no performance or production memory claim.
