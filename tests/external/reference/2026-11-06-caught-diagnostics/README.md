# PostgreSQL caught error storage

[reference.zip](reference.zip), SHA-256
`29a157920824fc34e886d671bcdefa5e06a3594c586c2ade8ebde9f90c4eceef`,
retains two PostgreSQL 18.6 results for fixture 250 on macOS arm64. Extract it
and run `python3 verify-reference.py .` to check file hashes, results, zero
errors and bounded reference-server settings.

The first function catches 4,096 errors with 8 KiB details and returns the
first saved message: `4096:error 1`. The second verifies that nested handlers
and rethrow preserve the outer message, SQLSTATE, detail and hint. Absent
detail, hint and column name are empty text, rather than NULL.

The archive includes SQL, runner, commands, raw output, logs, binary hashes
and provenance. The guarded Unix-socket probe used 16 MiB shared buffers,
1 MiB work memory and five connections. pos3ql runtime and allocation checks
run separately on CI; these results make no performance or production memory
claim.
