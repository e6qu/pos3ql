# PostgreSQL saved output values

[reference.zip](reference.zip), SHA-256
`e3f0db03fe1b669708a622e8dfd81735efedc0e92a97ea767a1156fab1e0903d`,
retains six PostgreSQL 18.6 result rows for fixture 261 on macOS arm64.
Extract it and run `python3 verify-reference.py .` to check file hashes,
results, empty diagnostics and the bounded server settings.

A function repeatedly shadows its text output with nested integer loop counters.
Its 1,537 rows retain the text output and exact incrementing counts. A caught
error preserves previously returned rows and local assignments; a later NULL
output stays NULL. Composite outputs retain field names, field types and arrays
with negative lower bounds. The native test uses the same SQL in both transaction
modes at the original statement capacity.

The archive includes SQL, runner, commands, raw output, server logs, binary hashes
and provenance. The guarded Unix-socket probe used 16 MiB shared buffers, 1 MiB
work memory, five connections and a five-second statement deadline. This records
PostgreSQL behavior; pos3ql execution and memory checks run separately on CI.
It is not a performance measurement.
