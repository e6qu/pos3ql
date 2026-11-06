# PostgreSQL local initialization order

[reference.zip](reference.zip), SHA-256
`3b5adf940da700c3b39aea17873befe258ebb8362d03c308e112f37e74922922`,
retains three PostgreSQL 18.6 results for fixture 252 on macOS arm64. Extract it
and run `python3 verify-reference.py .` to verify file hashes, results, zero
errors and bounded reference-server settings.

Named input and output parameters are established before local initializers.
A constant initializer increments a sequence once per call; a later NOT NULL
local reads that constant and the input parameter. An uninitialized local is
NULL, and a loop counter shadows and then restores another initialized local.
Calls with inputs 10 and 20 return `17|1:ok:true` and `28|2:ok:true`; the sequence
ends at 2.

The archive includes SQL, runner, commands, raw output, logs, binary hashes
and provenance. The guarded Unix-socket probe used 16 MiB shared buffers,
1 MiB work memory and five connections. pos3ql runtime and allocation checks
run separately on CI; these observations make no performance or production
memory claim.
