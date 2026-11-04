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

[inline-key-names.zip](inline-key-names.zip), SHA256
`a1479a4b8e119a1e64c941b0a28d8915ca54c6510592c6e249c216b1818611b9`,
records fixture `225_inline_key_names.sql`: long and UTF-8 names, schema
collisions, retained names after table and column renames, rebuilding and
dropping by the saved name. Extract it and run
`python3 verify-inline-key-names.py .` to verify 10 hashes and nine exact rows.

[index-parent-owner.zip](index-parent-owner.zip), SHA256
`b6bd11b6e07de1b723ffe1a041e5b7316215d2b746fd0a6e955a30a48f015723`,
records fixture `226_index_parent_ownership.sql`: table and materialized-view
owner changes reach their indexes, both before commit and after rollback.
Extract it and run `python3 verify-index-parent-owner.py .` to verify 10 hashes
and seven exact rows. Both archives retain exact SQL, raw results, commands and
binary/source hashes; their fixtures were uncommitted at revision `8800db18`.

The owner propagation rule excludes retired, non-live indexes. PostgreSQL 18.6's
[ATExecChangeOwner](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/tablecmds.c#L15289-L15294)
uses `RelationGetIndexList`, which
[omits indexes with indislive=false](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/cache/relcache.c#L4570-L4577).
The native delayed-publication test cancels a rebuild after retirement, then
checks that live user and large-value indexes follow the new owner while non-live
ones retain the old owner, across rollback, journal recovery and checkpoint recovery.
This additional lifetime case is derived from the pinned source; it is not an
observation recorded in the owner archive above.

Table-wide rebuilding obtains the same live-index list
([index.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/index.c#L3750)).
Invalidating an index clears validity while retaining its readiness and lifetime
flags; swapping a replacement must preserve those predecessor flags
([index.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/index.c#L1530-L1553)).
The native delayed-receipt tests exercise an unbuilt predecessor and a retired
index, including explicit repair under a changed parent owner. These cases are
source-derived checks alongside the archived PostgreSQL observations.

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

[key-namespace-reference.zip](key-namespace-reference.zip), SHA256
`3f215990fbb19442e889d4651d25f249e1a0f5171338714351f5b50b0d2aecac`,
records 17 exchanges between two PostgreSQL 18.6 sessions. An uncommitted
`a_b(c UNIQUE)` already reserves `a_b_c_key`: another session creating
`a(b_c UNIQUE)` immediately chooses `a_b_c_key1` and retains it even if the
first transaction rolls back. Committed table and domain CHECK names also
force a suffix within the schema; another schema is independent. This matches
[the pinned name-selection source](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/indexcmds.c#L2431-L2515).
The archive retains exact SQL, raw exchanges, runner, commands and binary hashes
at source revision `8800db18` with the naming changes uncommitted. Extract it and
run `python3 verify-key-namespace.py .` to verify 10 hashes and 11 observed name
rows. This qualifies PostgreSQL behavior; final pos3ql CI must still pass.

[owner-event-reference.zip](owner-event-reference.zip), SHA256
`5e9eac2fa268f34f3c7b40bbded460a454cad6659812edeb751dc6e0002c6bf2`,
records fixture `227_owner_event_commands.sql` on PostgreSQL 18.6. Changing a
table or materialized view's owner reports exactly the named object to the end
callback, including when it has indexes. Repeating the same owner change still
reports one command. Child ownership updates add no commands. The fixture was
uncommitted at source revision `8800db18`. Exact SQL, output, commands, binary
hashes and provenance are retained. Extract it and run
`python3 verify-owner-event.py .` to verify 10 hashes and four output rows.

[pending-key-namespace-reference.zip](pending-key-namespace-reference.zip), SHA256
`06b7846ca825da6e8a178efecdfa25c2a5851807f4b8414359f0fa818b74b30d`,
records the constraint visibility distinction in 26 two-session exchanges.
Another transaction's uncommitted table or domain CHECK named `a_b_c_key` does
not force a suffix for `a_b(c UNIQUE)`. The chosen name remains unchanged after
the first transaction commits or rolls back. This differs from an uncommitted
index relation, which already reserves its name. PostgreSQL's
[constraint-name scan](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/pg_constraint.c#L425-L459)
uses the catalog snapshot. Exact SQL, raw results, commands and binary hashes
are retained. Extract it and run `python3 verify-pending-key-namespace.py .`
to verify 10 hashes and 12 observed name rows.

[generated-key-names-reference.zip](generated-key-names-reference.zip), SHA256
`ffc16514e51dad3a16615e6cdf6083f45e95c2dc25d5ab9a42f2733fa38fe127`,
records fixture `228_generated_key_names.sql` on PostgreSQL 18.6. Generated
multi-column unique and exclusion names avoid existing relations and schema
constraint names. Foreign-key names avoid constraint names while ignoring
unrelated relation names. Copied CHECK names remain unchanged; copied and
partitioned unique keys choose free names. Long ASCII and Unicode names stay
within PostgreSQL's byte limit and survive concurrent index rebuilding.
The fixture was uncommitted at source revision `8f597543`. Exact SQL, raw output,
commands, binary hashes and provenance are retained. Extract it and run
`python3 verify-generated-key-names.py .` to verify 10 hashes and 13 output rows.

[reordered-index-links-reference.zip](reordered-index-links-reference.zip), SHA256
`57977a34d40cc8154779d4a1c0623d8fa4aab888e22df5f68fb9b313dbc2bea3`,
records fixture `229_reordered_partition_index_links.sql` on PostgreSQL 18.6.
An attached partition can store columns in a different order from its parent.
Its primary and unique constraints retain their physical column numbers while
linking to the correct parent constraint and index. Concurrent rebuilding
preserves those links and constraint identifiers, replaces the child index
identifiers and leaves rows intact. The fixture was uncommitted at source
revision `6e87c94c`. Extract it and run `python3 verify-reordered-index-links.py .`
to verify the retained SQL, raw output, provenance and commands against 10 hashes
and six output rows.

[planner-drop-reference.zip](planner-drop-reference.zip), SHA256
`79e6f1c7969477ee029e69f951daa8ffef616352ac7e32a3509b901315579520`,
records fixtures `230_planner_constant_order.sql` and `231_index_drop_undo.sql`
on PostgreSQL 18.6. It confirms projection and type errors before constant
evaluation, integer overflow before a later division error, skipped later CASE conditions,
LIKE escape expressions using columns, functions and subqueries, and declared
NULL parameter types. Rolling back a table drop preserves an earlier explicit index drop; full rollback
restores each original index once. These fixtures were uncommitted at revision
`a166c2ac`. Extract the archive and run `python3 verify-planner-drop.py .` to
check 12 file hashes, 25 result rows and the six expected error codes against
the exact SQL, raw output, commands and binary provenance. This is reference
behavior, not a passing pos3ql or performance result.

[planner-metadata-reference.zip](planner-metadata-reference.zip), SHA256
`daa7e45f1003d2e85a00535ce376491c97c4311f428910cdf484a204328a5c97`,
records fixture `232_planner_record_metadata.sql` and the PostgreSQL 18.6
B-tree/hash catalog inventory. Anonymous record fields, composite-domain array
elements and `RETURNING` old/new aliases retain their types and values. Reusing
an index name preserves each index's own comment. A constant true string
comparison discards a later overflow; `length(DISTINCT 'x')` reports `42809`.
The inventory contains 85 classes and 69 families, including `name_ops`' declared
key type. Exact SQL, commands, binary hashes and raw results were captured with
these repairs uncommitted at `fcd4f8cc`. Extract it and run
`python3 verify-planner-metadata.py .` to verify file hashes, 175 result rows and
seven expected error codes. These observations qualify the PostgreSQL reference;
the final pos3ql CI run must establish matching behavior.

[index-materialization-reference.zip](index-materialization-reference.zip), SHA256
`05f117e10f26c73124e9d8b58a7fe7ea1459bacda240d036e41579f510194809`,
records PostgreSQL 18.6 inline primary/unique indexes before and after a blocking
tablespace rebuild and concurrent replacement. Fixture `233_inline_index_materialization.sql`
returns 11 rows: each index appears once and retains its primary/unique flags.
A separate three-session probe records 35 protocol exchanges showing replacement
publication before an earlier writer finishes; cancellation retains that index
and the original constraint. Both ordinary and deferrable keys are covered.
Exact SQL, raw exchanges, commands, binary/source hashes and verifiers are retained.
The catalog fixture was uncommitted at `82c3d71d`; the writer probe ran at that
revision. Extract it and run `python3 verify-index-materialization.py .` to
verify both references. These are PostgreSQL observations, not pos3ql runtime
or performance qualification.

[nested-query-reference.zip](nested-query-reference.zip), SHA256
`f29b17c1ea0982bc9efde9e49ee6c4130e666653cc862637fdea9c4b71f31c5d`,
records fixture `234_nested_query_and_index_metadata.sql` on PostgreSQL 18.6:
14 result rows and six deliberate errors cover nested `ts_rewrite` queries,
record fields, constant-error order, index-comment rename rollback and partitioned
index kinds. Exact SQL, raw results, commands and binary/source hashes are retained;
the fixture was uncommitted at `36623559`.
The archive also preserves the original native crash reproduction from
[the historical diagnostic](https://github.com/e6qu/pos3ql/actions/runs/41870892426/job/126721297240).
Source `8efe894d` reproduced `SIGSEGV` in `InputAttempt::update_frame` while
`full_text_query_rewrite_and_vector_unnest_are_typed` ran a nested query.
The original 845-test partition, Rust 1.99.1, cargo-llvm-cov 0.9.1, 128 MiB
thread stack and ordinary test parallelism were retained. Extract the archive and
run `python3 verify-nested-query.py .` to verify 16 file hashes, reference rows,
errors and the crash's source, tool and stack identities. The historical failure
diagnoses the lifetime defect; final-head correctness still requires ordinary CI.

[implicit-index-ddl-reference.zip](implicit-index-ddl-reference.zip), SHA256
`a8ffd552c78f84ca9869779b6548ed144c5388c3c549fee00211164e38174852`,
records fixture `235_implicit_index_ddl.sql` on PostgreSQL 18.6. Primary, unique
and exclusion indexes can be renamed before any rebuild. Their identifiers and
comments survive rollback; direct and procedural index settings persist. An
unauthorized role gets `42501`; a real relation-name collision raises `42P07`.
Exact SQL, eight result rows, commands and binary/source
hashes were captured with the fixture uncommitted at `659218e3`. Extract it and
run `python3 verify-implicit-index-ddl.py .` to verify all 12 file hashes and the
observed results. This is reference behavior; pos3ql still needs passing CI.
