# PostgreSQL input preparation reference

[planner-procedure-properties.json](planner-procedure-properties.json) separately
records the pinned catalog sources and generator for all 3,397 planner procedure
facts. This inventory includes internal operator support without exposing new
SQL-callable functions. It supplements the historical runtime archives below.

This archive records PostgreSQL 18.6 behavior used by the native and differential
regressions. CI must test the final PR revision before merge.

[loop-reference.zip](loop-reference.zip), SHA256
`16bdad10534665025b24f92ae155d1aceafb6df777e684c6e56d6b8c55bb61f5`,
records implicit integer loop variables, shadowed names, retained query and array
targets, empty record results, range errors and exception matching on PostgreSQL
18.6. For example, a numeric loop leaves an outer variable unchanged, while a
query loop retains its last assigned value. It includes the exact SQL, raw
results, runner, commands, binary hashes and provenance. Extract it and run
`python3 verify-loop-reference.py .` to verify the evidence. This establishes
reference behavior; the matching pos3ql tests must still pass in CI.

[event-drop-reference.zip](event-drop-reference.zip), SHA256
`2c585c8d1d4394bb90eb689fbc0ee456d08ba4f0e45da1606a4d1a0127ab5de2`,
records search-path selection and original-versus-dependent DROP events on the
same PostgreSQL build. Dropping `shared_base` selects the first schema's table;
another schema's dependent view is reported separately, and its unrelated table
and foreign key survive. The archive includes exact SQL, raw results, commands,
binary hashes and provenance. Extract it and run
`python3 verify-event-drop-reference.py .` to verify the evidence.

[constraint-reference.zip](constraint-reference.zip), SHA256
`bba01e9132f612f1dd77ff456b678e9f6e41fa4bf98170ecb1453451be3b8e08`,
records concurrent reindexing of primary, unique, attached and partitioned keys.
Constraint identifiers and comments survive while backing indexes change;
exclusion indexes remain unchanged during a table-wide concurrent rebuild.
The reference also checks dependency links and DROP reporting after another
index was already dropped in the same transaction. Exact SQL, raw results,
commands, binary hashes and provenance are included. Extract it and run
`python3 verify-constraint-reference.py .` to verify the evidence.

[reindex-reference.zip](reindex-reference.zip), SHA256
`31690d72db894dd78d372ba37205c2dbd8590f889c123ab8121a3d6208a86fb3`,
retains six PostgreSQL 18.6 probes covering rebuild eligibility, event callbacks,
TOAST indexes and invalid-index repair. For example, a table-wide concurrent
rebuild skips an invalid index with a warning; an explicit index rebuild repairs
it after duplicate rows are removed. TOAST is PostgreSQL's separate storage for
large values: its relation keeps its identity while its index changes. The
archive preserves all 40 output rows, deliberate errors and warnings, exact SQL,
commands and source/binary hashes. Extract it and run
`python3 verify-reindex-reference.py .` to verify its 62 hashed files. Its recorded
source revision is `5119b065`; the reference SQL is preserved separately.

PostgreSQL retains index addresses across each table's internal commits
([indexcmds.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/indexcmds.c#L3235))
and runs one outer end callback
([utility.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/tcop/utility.c#L1830)).
The callback helper reads current identities and skips missing objects
([event_trigger.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/event_trigger.c#L1988)).
An earlier index can therefore be renamed or dropped while a later table waits;
the event list must not preserve its old name or identify a reused slot.

[toast-reference.zip](toast-reference.zip), SHA256
`42ede35ea6e1b88c6747c98003e78e1e8dc422ccae58f4dedca1d35ec7276331`,
records the exact `222_toast_index_identities.sql` fixture on the same PostgreSQL
build. It verifies index flags, key types, dependency links and the replacement
index's identity. The fixture was uncommitted at the recorded `5119b065` source
revision; its exact executed hash is retained. Extract it and run
`python3 verify-toast-reference.py .` to verify 10 hashes and 11 output rows.
These archives establish PostgreSQL behavior; matching pos3ql tests must pass CI.

[indexdef-reference.zip](indexdef-reference.zip), SHA256
`d90312ba1dcbc445a17f2817ae3e718c79941b1cfdfc94cfa0bd409f4dbd5b38`,
records the full TOAST index definition and individual key names before and after
a concurrent table rebuild on the same PostgreSQL build. Its external probe was
uncommitted at source revision `5119b065`; exact SQL, commands, results and hashes
are preserved. Extract it and run `python3 verify-indexdef-reference.py .` to
verify 10 hashes and both output rows.

[deferrable-reference.zip](deferrable-reference.zip), SHA256
`5a1df9e586f40046bf9714bbaad6cec211fd3c86dddd43b74506dd0009e8cec9`,
records the exact `223_deferrable_index_identities.sql` fixture. A deferred unique
constraint still allows duplicate values inside a transaction that is rolled
back. Concurrent and ordinary rebuilds preserve deferred checking and constraint
identity; the concurrent rebuild replaces the index identity. The fixture was
uncommitted at recorded revision `5119b065`. Extract it and run
`python3 verify-deferrable-reference.py .` to verify 10 hashes and four output
rows. These are PostgreSQL observations; final pos3ql CI must still pass.

[reindex-ownership.zip](reindex-ownership.zip), SHA256
`fa7d53631f6a502683ebdd9040ae3c4989ae6cf74687bac4cb7d84a37b63762f`,
records fixture `224_reindex_ownership.sql` on PostgreSQL 18.6. A superuser's
index creation and a maintenance role's rebuild both keep the table owner's
index ownership, including the large-value index. Table and column grants
survive. The fixture was uncommitted at source revision `c476eb30`; its exact
hash and raw results are retained. Extract it and run
`python3 verify-reindex-ownership.py .` to verify 10 hashes and four output rows.

[reference.zip](reference.zip) contains raw SQL output, the two-session records,
server/image identifiers, the checkout revision and all input hashes from the
[original runner artifact](https://github.com/e6qu/pos3ql/actions/runs/41685422665/artifacts/13257235290).
[provenance.json](provenance.json) records its verified ZIP checksum, source
revision, synthetic merge parents/tree and source-file hashes. The synthetic
merge tree matched the recorded PR source tree. The original probe sources and
workflow are retained beside this file; differential fixtures remain in their
ordinary test directory.

The probes show when a user-defined constraint runs during function preparation,
cursor declaration/fetch, casts and cache invalidation. For example, a typed
child array required two planning checks and one additional runtime check.
ANALYZE refreshed a dependent function when table estimates changed, while
unchanged estimates preserved it. Two-session output records immediate delivery,
local rollback and commit delivery separately. Role tests intentionally produce
permission errors. These are SQL effect counts.

To reproduce, use the pinned PostgreSQL service and initialization options from
[workflow.yml](workflow.yml). Run [the SQL probe](routine-input-oracle.sql), then
[the two-session probe](routine-input-oracle.py) with the container identifier and
an output-file path. Run the differential fixtures in the order recorded by
`inputs` in provenance.json. Match the recorded locale, encoding, server version
and image before comparing effects. The archived workflow records the original
paths of the probes; their retained copies are beside this README.

[boundary-reference.zip](boundary-reference.zip) adds a focused PostgreSQL 18.6
Homebrew reference on macOS arm64, using UTF8 and the C locale. Its SHA256 is
`f541e5d7fa178a8c463f92c770abcfb815cdca10e6a225b962529fe7389af110`.
It retains the SQL, runner, exact commands, binary/source hashes, raw output and
an independent verifier. Extract it, then run
`python3 verify-boundary-reference.py .` inside the extracted directory.
These observations qualify PostgreSQL semantics; final pos3ql CI must establish
that the implementation matches them.

For example, declaring a one-row cursor does not issue its `nextval` until
fetching, and `FETCH 0` leaves the sequence untouched. Declaring it without a
scroll option reports a non-scrollable cursor. Checked defaults and prepared
values are reused without repeating their constraint. A same-owner change still
reports a DDL event. Foreign-server and wrapper object labels are respectively
`server` and `foreign-data wrapper`, including object identification and address
lookup. The probe also records left-to-right query-analysis errors and initialized
function costs that differ from bootstrap declarations.

[owner-reassignment.zip](owner-reassignment.zip), SHA256
`55717bafd3669fd59a5118d5fd572160b48998045d7476e367296030e2369e40`,
records the same PostgreSQL build moving a wrapper, server and collation to a new
owner, restoring them on rollback and changing them on commit. The destination
role can also appear in the source list; reassigning a role's objects to itself
finishes without changing their owner. SQL, commands, hashes, raw output and
`verify-owner-reassignment.py` are retained inside the archive. Extract it and
run `python3 verify-owner-reassignment.py .` to verify the recorded observations.

[protocol-reference.zip](protocol-reference.zip), SHA256
`a0f41b53f2a76d00350d0030dfebd8e739b3393485ed7f4b23a67a6e8204bacb`,
retains 69 raw protocol exchanges on that PostgreSQL build, their runner and
provenance. Startup cancellation keys are omitted at capture. A duplicate portal
Bind fails the explicit transaction. Database
creation publishes before `Sync`; concurrent index commands expose intermediate
catalog states before their final transaction ends. Bound integer and text
parameters skip unreached function checks. Extract it and run
`python3 verify-protocol-reference.py .` to check file hashes and these observations.

[execution-reference.zip](execution-reference.zip), SHA256
`ad4cfd16a752ff34ec8ab52255f9d1a6b3326cfb398938476bde9305fb4c2756`,
retains the cursor-source and procedural-query probes. Two comma-separated VALUES
rows allow backward fetching by default; one VALUES row and explicit UNION ALL
do not. Declaring these cursors does not run their sequence calls. Procedural
queries reuse checked inputs, and a caught `RETURN QUERY` error preserves rows
already produced before its handler runs. Static SELECT INTO stops after one row,
or two with STRICT; dynamic EXECUTE INTO
evaluates the full query. The archive includes raw output, exact
sources, provenance and `verify-execution-reference.py`; extract it and run
`python3 verify-execution-reference.py .` to verify the observations and 21 hashes.
