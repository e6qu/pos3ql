# PostgreSQL lexical scopes and constant cursor targets

[reference.zip](reference.zip), SHA-256
`ff108bd6c9fdc4350d7a5f760f3bc43644df98fc0af78b169a67b0fc650596f5`,
retains five PostgreSQL 18.6 result rows and six expected errors for fixtures
257–258 on macOS arm64. Extract it and run `python3 verify-reference.py .`
to verify hashes, results, error codes, compile/runtime error locations and
bounded reference-server settings.

A numeric counter named `SQLSTATE` or `SQLERRM` inside a handler has integer
values and type. Leaving the loop restores the handler's text variable;
a handler inside a loop introduces its own text variable. Outside a handler,
unquoted `SQLSTATE` needs an existing binding, while quoting it allows a new
counter.

Assignments to handler variables and `FETCH` into constants fail with `22005`
when the function is created. Opening a constant cursor with an existing name
is allowed. A NULL constant fails with `22005` at `OPEN`, when it would need
to receive a generated name. The archive records five creation errors and
that runtime error; invalid function definitions leave no catalog entries.
The named `error_in_assignment` condition catches the runtime failure.

SQL, runner, commands, raw output, logs, binary hashes and provenance are
retained together. The guarded Unix-socket probe used 16 MiB shared buffers,
1 MiB work memory and five connections. These observations make no performance
claim; pos3ql execution and allocation checks run separately on CI.
