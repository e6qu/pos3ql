# PostgreSQL input preparation reference

This archive records PostgreSQL 18.6 behavior used by the native and differential
regressions. CI must test the final PR revision before merge.

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
