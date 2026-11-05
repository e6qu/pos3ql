# PostgreSQL loop row values

[reference.zip](reference.zip), SHA-256
`e7dfc87dad740c0b48057273ef4a60698eafee6498dafd2ce2f6543399911b5e`,
retains seven PostgreSQL 18.6 results for fixture 247 on macOS arm64. Extract it
and run `python3 verify-reference.py .` to verify hashes, results, zero errors
and bounded reference-server settings.

A record variable receives named fields from a static query, a dynamic query
with an explicit parameter, and a function. An empty query keeps its field names
with null values; an empty array leaves its iteration variable unchanged.
A value copied from an earlier record remains usable after later loops. A
sequence in the function advances once, confirming that retaining and reading
loop rows does not repeat the call.

SQL, the runner, commands, output, logs, binary hashes and source provenance
are archived. The guarded Unix-socket probe used 16 MiB shared buffers, 1 MiB work
memory and five connections. pos3ql runtime and allocation checks run separately
on CI. These observations make no performance or production memory claim.
