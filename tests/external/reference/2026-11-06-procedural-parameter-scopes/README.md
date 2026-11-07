# PostgreSQL parameter and local scopes

[reference.zip](reference.zip), SHA-256
`0da8b0441cdf8cd762f6f8a3643d7036c5487f6033e440066f53670411d8ca7d`,
retains eight PostgreSQL 18.6 result rows for fixture 259 on macOS arm64.
Extract it and run `python3 verify-reference.py .` to verify file hashes,
results, empty diagnostics and the bounded server settings.

Parameters named `SQLSTATE` and `SQLERRM` work outside error handlers.
Handlers introduce text variables with those names; an inner numeric loop
can introduce its own integer counter. Leaving either scope restores the
previous binding. Quoted uppercase names remain distinct.

A body declaration may shadow a parameter, including with a different type.
Reads, assignments, SELECT/EXECUTE INTO, numeric counters and captured cursor
inputs select the body local. Output parameters keep their own slots: changing
a shadowing local leaves the original scalar, table or procedure output alone.

The archive includes SQL, runner, commands, raw output, server logs, binary
hashes and provenance. The guarded Unix-socket probe used 16 MiB shared buffers,
1 MiB work memory and five connections. It establishes SQL behavior, with no
pos3ql execution or performance claim; native checks run separately on CI.
