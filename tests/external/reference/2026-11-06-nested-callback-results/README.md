# PostgreSQL nested function results

[reference.zip](reference.zip), SHA-256
`6dab2d669a93d44dd0f1ef5266db416f8315d0f9092e27f0d74c05623b4929a6`,
retains four PostgreSQL 18.6 result rows for fixture 260 on macOS arm64.
Extract it and run `python3 verify-reference.py .` to verify file hashes,
results, empty diagnostics and the bounded server settings.

A SQL function summarizes 4,096 text rows from a procedural function. Calling
it twice in one SELECT returns both summaries, with two sequence increments
and two audit writes. A later call returns the same summary and adds exactly
one increment and audit row. The native test uses the same SQL in both
transaction modes at the original statement-memory capacity; it checks that
completed nested result images do not accumulate across calls.

The archive includes SQL, runner, commands, raw output, server logs, binary
hashes and provenance. The guarded Unix-socket probe used 16 MiB shared buffers,
1 MiB work memory, five connections and a five-second statement deadline.
This establishes PostgreSQL SQL behavior; pos3ql execution and memory checks
run separately on CI. It is not a performance measurement.
