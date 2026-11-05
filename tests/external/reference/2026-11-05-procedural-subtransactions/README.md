# PostgreSQL procedural error cleanup

[reference.zip](reference.zip), SHA-256
`ceae7707c7fdc55b58870fd5049b220402984d87321e6e890e55c026212037cb`,
records PostgreSQL 18.6 results for differential fixtures 236–240 and the foreign-write error-cleanup oracle.

For example, a function can add a column and then catch a deliberate error.
Its handler must see the original table, its earlier settings and its updated
local variables. A rolled-back cursor opening releases its name; moving an
older cursor keeps its new position. Failed trigger writes must leave no queued
deferred trigger, while changes to `NEW` and the statement's transition rows
remain available to the handler. Identity checks include session authorization
and a function executing with its owner's permissions.

The archive retains exact SQL, all 37 result rows, three deliberate errors
(`22012`, `22012`, `55000`), commands, runner, PostgreSQL binary hashes and
provenance. Extract it and run
`python3 verify-procedural-subtransactions.py .` to verify file hashes, inputs,
results, errors and the recorded server configuration. The probe used a small
Unix-socket-only PostgreSQL instance under the local resource guard.

The foreign-write oracle runs the same SQL on a local reference table. CI also
runs it through pos3ql's foreign table against actual PostgreSQL, checking a
connection first opened inside a failed block and nested remote savepoints.

This establishes PostgreSQL reference behavior. Matching pos3ql native tests,
allocation checks, empty-cache journal/checkpoint recovery and full final-head
CI still have to pass before the implementation is qualified.

PostgreSQL documents the distinction between preserved variables and rolled-back
database changes in [error trapping](https://www.postgresql.org/docs/18/plpgsql-control-structures.html#PLPGSQL-ERROR-TRAPPING),
and cursor creation versus motion in [savepoint rollback](https://www.postgresql.org/docs/18/sql-rollback-to.html).
