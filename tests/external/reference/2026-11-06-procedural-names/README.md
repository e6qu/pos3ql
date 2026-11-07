# PostgreSQL procedural identifier case

[reference.zip](reference.zip), SHA-256
`12b84fc91bae4faa8ee712fab0d37755f6ddfc56037853f04f0e96b855064ef7`,
retains five PostgreSQL 18.6 results for fixture 253 on macOS arm64. Extract it
and run `python3 verify-reference.py .` to verify hashes, results, zero errors
and bounded reference-server settings.

Unquoted names fold to lower case; quoted names retain their spelling.
`SQLERRM`, `FOUND` and `TG_NAME` remain different from their uppercase quoted
forms. Mixed-case and space-containing local names remain distinct, loop
counters and labels fold consistently, and record fields preserve quoted case.
The fixture catches missing-variable errors and returns stable results.

The archive includes SQL, runner, commands, raw output, logs, binary hashes
and provenance. The guarded Unix-socket probe used 16 MiB shared buffers,
1 MiB work memory and five connections. pos3ql runtime and allocation checks
run separately on CI; these observations make no performance or production
memory claim.
