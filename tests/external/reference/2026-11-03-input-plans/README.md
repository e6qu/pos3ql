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
