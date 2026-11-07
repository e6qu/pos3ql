# PostgreSQL procedural control flow

[reference.zip](reference.zip), SHA-256
`8ecc453882febfe811cfb620568648e25deec5408bf5df60991de039f4f4cb9e`,
retains PostgreSQL 18.6 results for fixture 246 on macOS arm64. Extract it and
run `python3 verify-reference.py .` to check all file hashes, 12 result rows,
zero errors and the reference server settings.

An outer numeric loop shadows a text variable. A labelled continuation crosses
an inner query loop and an exception handler; leaving the loop restores the
text variable and preserves the accumulated total. Nested handlers keep their
own diagnostic code, and rethrowing after an inner handler restores the outer
error. Array slices, nested array loops, WHILE, LOOP and early RETURN complete
the comparison. A sequence confirms that the handler's effect and the unmatched CASE operand each occur once.
Unmatched searched, simple and null CASE statements without ELSE raise `20000`;
the caught message and hint match PostgreSQL. Matched branches and explicit ELSE
complete successfully. The upstream [executor](https://github.com/postgres/postgres/blob/REL_18_6/src/pl/plpgsql/src/pl_exec.c#L2472-L2477)
and [error codes](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/errcodes.txt#L140-L142)
define this boundary.

The archive includes SQL, the runner, commands, raw output, logs, binary hashes
and source provenance. Its guarded Unix-socket server used 16 MiB shared buffers,
1 MiB work memory and five connection slots. This is reference behavior;
pos3ql native and allocation checks run separately on CI. These observations
make no performance or production memory claim.
