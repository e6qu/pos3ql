# PostgreSQL procedural control flow

[reference.zip](reference.zip), SHA-256
`40c317514c31705f7707d934f407620889e70b5660f4b6395173cd8846791da1`,
retains PostgreSQL 18.6 results for fixture 246 on macOS arm64. Extract it and
run `python3 verify-reference.py .` to check all file hashes, seven result rows,
zero errors and the reference server settings.

An outer numeric loop shadows a text variable. A labelled continuation crosses
an inner query loop and an exception handler; leaving the loop restores the
text variable and preserves the accumulated total. Nested handlers keep their
own diagnostic code, and rethrowing after an inner handler restores the outer
error. Array slices, nested array loops, WHILE, LOOP and early RETURN complete
the comparison. A sequence confirms that the handler's effect occurs once.

The archive includes SQL, the runner, commands, raw output, logs, binary hashes
and source provenance. Its guarded Unix-socket server used 16 MiB shared buffers,
1 MiB work memory and five connection slots. This is reference behavior;
pos3ql native and allocation checks run separately on CI. These observations
make no performance or production memory claim.
