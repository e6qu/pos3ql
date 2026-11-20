# PostgreSQL input preparation reference

This directory records PostgreSQL 18.6 behavior that pos3ql's native and
differential regressions are checked against.

Unless an entry says otherwise, each archive under [Archives](#archives) was
captured on the same PostgreSQL 18.6 build and retains the exact SQL, raw results, runner and
commands, binary and source hashes, provenance and a verifier. To check one,
extract it and run `python3 VERIFIER .` inside the extracted directory. "Uncommitted
at revision X" means the fixture ran from a working tree based on commit X
before the fixture was committed; its exact executed hash is retained.

The archives establish PostgreSQL reference behavior only. They are not pos3ql
test passes or performance results: the matching pos3ql tests must pass CI on
the final PR revision before merge.

## Planner procedure inventory

[planner-procedure-properties.json](planner-procedure-properties.json) records
the pinned catalog sources (`REL_18_6`) and generator for all 3,397 planner
procedure facts. The inventory includes internal operator support without
exposing new SQL-callable functions.

## Routine input probes

[reference.zip](reference.zip) contains raw SQL output, the two-session records,
server/image identifiers, the checkout revision and all input hashes from the
[original runner artifact](https://github.com/e6qu/pos3ql/actions/runs/41685422665/artifacts/13257235290).
[provenance.json](provenance.json) records its verified ZIP checksum, source
revision, synthetic merge parents/tree and source-file hashes; the synthetic
merge tree matched the recorded PR source tree. The original probe sources and
workflow are retained beside this file.

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
`inputs` in provenance.json, and match the recorded locale, encoding, server
version and image before comparing effects.

## Archives

**[loop-reference.zip](loop-reference.zip)**, SHA256
`16bdad10534665025b24f92ae155d1aceafb6df777e684c6e56d6b8c55bb61f5`, verifier
`verify-loop-reference.py`: implicit integer loop variables, shadowed names,
retained query and array targets, empty record results, range errors and
exception matching. For example, a numeric loop leaves an outer variable
unchanged, while a query loop retains its last assigned value.

**[event-drop-reference.zip](event-drop-reference.zip)**, SHA256
`2c585c8d1d4394bb90eb689fbc0ee456d08ba4f0e45da1606a4d1a0127ab5de2`, verifier
`verify-event-drop-reference.py`: search-path selection and original-versus-dependent
DROP events. Dropping `shared_base` selects the first schema's table; another
schema's dependent view is reported separately, and its unrelated table and
foreign key survive.

**[constraint-reference.zip](constraint-reference.zip)**, SHA256
`bba01e9132f612f1dd77ff456b678e9f6e41fa4bf98170ecb1453451be3b8e08`, verifier
`verify-constraint-reference.py`: concurrent reindexing of primary, unique,
attached and partitioned keys. Constraint identifiers and comments survive while
backing indexes change; exclusion indexes remain unchanged during a table-wide
concurrent rebuild. It also checks dependency links and DROP reporting after
another index was already dropped in the same transaction.

**[reindex-reference.zip](reindex-reference.zip)**, SHA256
`31690d72db894dd78d372ba37205c2dbd8590f889c123ab8121a3d6208a86fb3`, verifier
`verify-reindex-reference.py` (62 hashed files): six probes covering rebuild
eligibility, event callbacks, TOAST indexes and invalid-index repair, with all
40 output rows and deliberate errors and warnings. For example, a table-wide
concurrent rebuild skips an invalid index with a warning; an explicit index
rebuild repairs it after duplicate rows are removed. TOAST is PostgreSQL's
separate storage for large values: its relation keeps its identity while its
index changes. Recorded source revision `5119b065`; the reference SQL is
preserved separately.

PostgreSQL retains index addresses across each table's internal commits
([indexcmds.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/indexcmds.c#L3235))
and runs one outer end callback
([utility.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/tcop/utility.c#L1830)).
The callback helper reads current identities and skips missing objects
([event_trigger.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/event_trigger.c#L1988)).
An earlier index can therefore be renamed or dropped while a later table waits;
the event list must not preserve its old name or identify a reused slot.

**[toast-reference.zip](toast-reference.zip)**, SHA256
`42ede35ea6e1b88c6747c98003e78e1e8dc422ccae58f4dedca1d35ec7276331`, verifier
`verify-toast-reference.py` (10 hashes, 11 output rows): fixture
`222_toast_index_identities.sql`, checking index flags, key types, dependency
links and the replacement index's identity. Uncommitted at revision `5119b065`.

**[indexdef-reference.zip](indexdef-reference.zip)**, SHA256
`d90312ba1dcbc445a17f2817ae3e718c79941b1cfdfc94cfa0bd409f4dbd5b38`, verifier
`verify-indexdef-reference.py` (10 hashes, both output rows): the full TOAST
index definition and individual key names before and after a concurrent table
rebuild. Its probe was uncommitted at revision `5119b065`.

**[deferrable-reference.zip](deferrable-reference.zip)**, SHA256
`5a1df9e586f40046bf9714bbaad6cec211fd3c86dddd43b74506dd0009e8cec9`, verifier
`verify-deferrable-reference.py` (10 hashes, four output rows): fixture
`223_deferrable_index_identities.sql`. A deferred unique constraint still allows
duplicate values inside a transaction that is rolled back. Concurrent and
ordinary rebuilds preserve deferred checking and constraint identity; the
concurrent rebuild replaces the index identity. Uncommitted at revision
`5119b065`.

**[reindex-ownership.zip](reindex-ownership.zip)**, SHA256
`fa7d53631f6a502683ebdd9040ae3c4989ae6cf74687bac4cb7d84a37b63762f`, verifier
`verify-reindex-ownership.py` (10 hashes, four output rows): fixture
`224_reindex_ownership.sql`. A superuser's index creation and a maintenance
role's rebuild both keep the table owner's index ownership, including the
large-value index; table and column grants survive. Uncommitted at revision
`c476eb30`.

**[inline-key-names.zip](inline-key-names.zip)**, SHA256
`a1479a4b8e119a1e64c941b0a28d8915ca54c6510592c6e249c216b1818611b9`, verifier
`verify-inline-key-names.py` (10 hashes, nine exact rows): fixture
`225_inline_key_names.sql`, covering long and UTF-8 names, schema collisions,
retained names after table and column renames, and rebuilding and dropping by
the saved name. Uncommitted at revision `8800db18`.

**[index-parent-owner.zip](index-parent-owner.zip)**, SHA256
`b6bd11b6e07de1b723ffe1a041e5b7316215d2b746fd0a6e955a30a48f015723`, verifier
`verify-index-parent-owner.py` (10 hashes, seven exact rows): fixture
`226_index_parent_ownership.sql`. Table and materialized-view owner changes
reach their indexes, both before commit and after rollback. Uncommitted at
revision `8800db18`.

The owner propagation rule excludes retired, non-live indexes. PostgreSQL 18.6's
[ATExecChangeOwner](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/tablecmds.c#L15289-L15294)
uses `RelationGetIndexList`, which
[omits indexes with indislive=false](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/cache/relcache.c#L4570-L4577).
Table-wide rebuilding obtains the same live-index list
([index.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/index.c#L3750)).
Invalidating an index clears validity while retaining its readiness and lifetime
flags; swapping a replacement must preserve those predecessor flags
([index.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/index.c#L1530-L1553)).
pos3ql's native delayed-publication and delayed-receipt tests check these
source-derived cases, which are not observations recorded in the archives: a
rebuild cancelled after retirement (live user and large-value indexes follow
the new owner, non-live ones keep the old owner, across rollback, journal
recovery and checkpoint recovery), and an unbuilt predecessor and a retired
index, including explicit repair under a changed parent owner.

**[boundary-reference.zip](boundary-reference.zip)**, SHA256
`f541e5d7fa178a8c463f92c770abcfb815cdca10e6a225b962529fe7389af110`, verifier
`verify-boundary-reference.py`: PostgreSQL 18.6 from Homebrew on macOS arm64,
UTF8 and the C locale. Declaring a one-row cursor does not issue its `nextval`
until fetching, and `FETCH 0` leaves the sequence untouched. Declaring it
without a scroll option reports a non-scrollable cursor. Checked defaults and
prepared values are reused without repeating their constraint. A same-owner
change still reports a DDL event. Foreign-server and wrapper object labels are
`server` and `foreign-data wrapper`, including object identification and
address lookup. It also records left-to-right query-analysis errors and
initialized function costs that differ from bootstrap declarations.

**[owner-reassignment.zip](owner-reassignment.zip)**, SHA256
`55717bafd3669fd59a5118d5fd572160b48998045d7476e367296030e2369e40`, verifier
`verify-owner-reassignment.py`: moving a wrapper, server and collation to a new
owner, restoring them on rollback and changing them on commit. The destination
role can also appear in the source list; reassigning a role's objects to itself
finishes without changing their owner.

**[protocol-reference.zip](protocol-reference.zip)**, SHA256
`a0f41b53f2a76d00350d0030dfebd8e739b3393485ed7f4b23a67a6e8204bacb`, verifier
`verify-protocol-reference.py`: 69 raw protocol exchanges; startup cancellation
keys are omitted at capture. A duplicate portal Bind fails the explicit
transaction. Database creation publishes before `Sync`; concurrent index
commands expose intermediate catalog states before their final transaction
ends. Bound integer and text parameters skip unreached function checks.

**[execution-reference.zip](execution-reference.zip)**, SHA256
`ad4cfd16a752ff34ec8ab52255f9d1a6b3326cfb398938476bde9305fb4c2756`, verifier
`verify-execution-reference.py` (21 hashes): cursor-source and procedural-query
probes. Two comma-separated VALUES rows allow backward fetching by default; one
VALUES row and explicit UNION ALL do not. Declaring these cursors does not run
their sequence calls. Procedural queries reuse checked inputs, and a caught
`RETURN QUERY` error preserves rows already produced before its handler runs.
Static SELECT INTO stops after one row, or two with STRICT; dynamic EXECUTE
INTO evaluates the full query.

**[key-namespace-reference.zip](key-namespace-reference.zip)**, SHA256
`3f215990fbb19442e889d4651d25f249e1a0f5171338714351f5b50b0d2aecac`, verifier
`verify-key-namespace.py` (10 hashes, 11 observed name rows): 17 exchanges
between two sessions. An uncommitted `a_b(c UNIQUE)` already reserves
`a_b_c_key`: another session creating `a(b_c UNIQUE)` immediately chooses
`a_b_c_key1` and retains it even if the first transaction rolls back. Committed
table and domain CHECK names also force a suffix within the schema; another
schema is independent. This matches
[the pinned name-selection source](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/indexcmds.c#L2431-L2515).
Captured at revision `8800db18` with the naming changes uncommitted.

**[owner-event-reference.zip](owner-event-reference.zip)**, SHA256
`5e9eac2fa268f34f3c7b40bbded460a454cad6659812edeb751dc6e0002c6bf2`, verifier
`verify-owner-event.py` (10 hashes, four output rows): fixture
`227_owner_event_commands.sql`. Changing a table or materialized view's owner
reports exactly the named object to the end callback, including when it has
indexes. Repeating the same owner change still reports one command; child
ownership updates add none. Uncommitted at revision `8800db18`.

**[pending-key-namespace-reference.zip](pending-key-namespace-reference.zip)**,
SHA256 `06b7846ca825da6e8a178efecdfa25c2a5851807f4b8414359f0fa818b74b30d`,
verifier `verify-pending-key-namespace.py` (10 hashes, 12 observed name rows):
26 two-session exchanges. Another transaction's uncommitted table or domain
CHECK named `a_b_c_key` does not force a suffix for `a_b(c UNIQUE)`, and the
chosen name is unchanged after the first transaction commits or rolls back.
This differs from an uncommitted index relation, which already reserves its
name. PostgreSQL's
[constraint-name scan](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/pg_constraint.c#L425-L459)
uses the catalog snapshot.

**[generated-key-names-reference.zip](generated-key-names-reference.zip)**,
SHA256 `ffc16514e51dad3a16615e6cdf6083f45e95c2dc25d5ab9a42f2733fa38fe127`,
verifier `verify-generated-key-names.py` (10 hashes, 13 output rows): fixture
`228_generated_key_names.sql`. Generated multi-column unique and exclusion
names avoid existing relations and schema constraint names. Foreign-key names
avoid constraint names while ignoring unrelated relation names. Copied CHECK
names remain unchanged; copied and partitioned unique keys choose free names.
Long ASCII and Unicode names stay within PostgreSQL's byte limit and survive
concurrent index rebuilding. Uncommitted at revision `8f597543`.

**[reordered-index-links-reference.zip](reordered-index-links-reference.zip)**,
SHA256 `57977a34d40cc8154779d4a1c0623d8fa4aab888e22df5f68fb9b313dbc2bea3`,
verifier `verify-reordered-index-links.py` (10 hashes, six output rows): fixture
`229_reordered_partition_index_links.sql`. An attached partition can store
columns in a different order from its parent. Its primary and unique
constraints retain their physical column numbers while linking to the correct
parent constraint and index. Concurrent rebuilding preserves those links and
constraint identifiers, replaces the child index identifiers and leaves rows
intact. Uncommitted at revision `6e87c94c`.

**[planner-drop-reference.zip](planner-drop-reference.zip)**, SHA256
`79e6f1c7969477ee029e69f951daa8ffef616352ac7e32a3509b901315579520`, verifier
`verify-planner-drop.py` (12 file hashes, 25 result rows, six expected error
codes): fixtures `230_planner_constant_order.sql` and
`231_index_drop_undo.sql`. Projection and type errors precede constant
evaluation, integer overflow precedes a later division error, later CASE
conditions are skipped, LIKE escape expressions may use columns, functions and
subqueries, and declared NULL parameter types are confirmed. Rolling back a table drop
preserves an earlier explicit index drop; full rollback restores each original
index once. Uncommitted at revision `a166c2ac`.

**[planner-metadata-reference.zip](planner-metadata-reference.zip)**, SHA256
`daa7e45f1003d2e85a00535ce376491c97c4311f428910cdf484a204328a5c97`, verifier
`verify-planner-metadata.py` (file hashes, 175 result rows, seven expected
error codes): fixture `232_planner_record_metadata.sql` and the B-tree/hash
catalog inventory (85 classes and 69 families, including `name_ops`' declared
key type). Anonymous record fields, composite-domain array elements and
`RETURNING` old/new aliases retain their types and values. Reusing an index
name preserves each index's own comment. A constant true string comparison
discards a later overflow; `length(DISTINCT 'x')` reports `42809`. Captured
with these repairs uncommitted at `fcd4f8cc`.

**[index-materialization-reference.zip](index-materialization-reference.zip)**,
SHA256 `05f117e10f26c73124e9d8b58a7fe7ea1459bacda240d036e41579f510194809`,
verifier `verify-index-materialization.py` (checks both references): inline
primary/unique indexes before and after a blocking tablespace rebuild and
concurrent replacement. Fixture `233_inline_index_materialization.sql` returns
11 rows: each index appears once and retains its primary/unique flags. A
separate three-session probe records 35 protocol exchanges showing replacement
publication before an earlier writer finishes; cancellation retains that index
and the original constraint. Ordinary and deferrable keys are covered. The
catalog fixture was uncommitted at `82c3d71d`; the writer probe ran at that
revision.

**[nested-query-reference.zip](nested-query-reference.zip)**, SHA256
`f29b17c1ea0982bc9efde9e49ee6c4130e666653cc862637fdea9c4b71f31c5d`, verifier
`verify-nested-query.py` (16 file hashes, reference rows, errors, and the
crash's source, tool and stack identities): fixture
`234_nested_query_and_index_metadata.sql`, whose 14 result rows and six
deliberate errors cover nested `ts_rewrite` queries, record fields,
constant-error order, index-comment rename rollback and partitioned index
kinds. Uncommitted at `36623559`. The archive also preserves the native crash
reproduction from
[the historical diagnostic](https://github.com/e6qu/pos3ql/actions/runs/41870892426/job/126721297240):
source `8efe894d` reproduced `SIGSEGV` in `InputAttempt::update_frame` while
`full_text_query_rewrite_and_vector_unnest_are_typed` ran a nested query, with
the original 845-test partition, Rust 1.99.1, cargo-llvm-cov 0.9.1, a 128 MiB
thread stack and ordinary test parallelism.

**[implicit-index-ddl-reference.zip](implicit-index-ddl-reference.zip)**, SHA256
`a8ffd552c78f84ca9869779b6548ed144c5388c3c549fee00211164e38174852`, verifier
`verify-implicit-index-ddl.py` (all 12 file hashes and the observed results):
fixture `235_implicit_index_ddl.sql`, eight result rows. Primary, unique and
exclusion indexes can be renamed before any rebuild. Their identifiers and
comments survive rollback; direct and procedural index settings persist. An
unauthorized role gets `42501`; a real relation-name collision raises `42P07`.
Uncommitted at `659218e3`.
