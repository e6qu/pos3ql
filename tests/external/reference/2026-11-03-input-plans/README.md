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
