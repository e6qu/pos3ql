# PostgreSQL loop scopes and input-check errors

[reference.zip](reference.zip), SHA-256
`0e27b050d090e698420f561b2969c0253864b44259f2b791b314d3a4007daddf`,
retains PostgreSQL 18.6 results on macOS arm64. Extract it and run
`python3 verify-reference.py .` to verify all archived hashes, results, error
codes and reference-server settings.

The loop fixture enters an inner numeric loop 16,384 times. Its first iteration
raises a caught error; later iterations continue. A text variable shadowed by
both counters returns to `outside`, and `FOUND` records that the outer loop ran.
Labelled `CONTINUE`/`EXIT` preserve output parameters; an early return keeps its
text result. The three result rows are:

```text
32767|outside|1|t
outside|5
returned:1:1
```

The input-validation fixture distinguishes a false domain CHECK from an
exception raised by its expression. It returns five validity results and
50 deliberate hard errors, including matching `23514` spellings from different
causes, custom application codes and cancellation. Scalar domains, domain arrays
and composite fields follow the same boundary.

Both resource-guarded probes used only a Unix socket, 16 MiB shared buffers,
1 MiB work memory and five connection slots. Scripts, SQL, commands, raw output,
server logs, binary hashes and provenance are retained. These are PostgreSQL
reference semantics; they do not measure pos3ql memory use or throughput.
pos3ql still needs native, allocation, Miri and full current-head qualification.
