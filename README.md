# pos3ql

pos3ql is a PostgreSQL-compatible database engine in Rust. SQL and the PostgreSQL v3 simple and extended protocols are the compatibility boundary. Durable data lives in provider-neutral object storage; RAM and local disk are bounded caches.

## Architecture

- PostgreSQL clients: psql, JDBC, Npgsql, psycopg, node-postgres, and pgx use the ordinary wire protocol, including catalog-typed text/binary Bind, Result, COPY, and set-returning integer/numeric/temporal output for implemented types.
- Durable state: immutable commit batches, immutable SST blocks, and compare-and-swap roots. A node can cold-start with an empty local disk.
- Object storage: the engine's production boundary is the common S3-compatible HTTP API implemented by MinIO and multiple object stores: direct signed requests, conditional PUT, full/ranged GET, paginated LIST, DELETE, and opaque-ETag compare-and-swap. pos3ql uses no vendor SDK, intermediary storage service, translation proxy, or endpoint-specific behavior. [Protocol subset, invariants, and qualification](docs/object-storage.md).
- Memory: all runtime memory is budgeted at startup. Pools and queues have fixed limits; exhaustion is an error.
- Determinism: the core is event-driven and runs under deterministic fault simulation.

## Durability

| Mode | Acknowledgement | Survives local-disk loss |
|---|---|---|
| `object_store = off` | local journal sync | no |
| `object_store = on` | immutable commit batch PUT and commit-head CAS | yes |

With object storage enabled, the server groups transactions completed in one reactor turn—including statements resumed after lock and object-read waits—publishes their immutable journal bytes, then advances a CAS commit head before releasing success responses. Checkpoints publish immutable table state through a separate CAS manifest. Recovery follows the commit head beyond that manifest; local disk is a cache. [The benchmark suite](docs/performance.md) measures the request shape, cache tiers, interference, and logical read-replica scaling against PostgreSQL 18.

## Status

The single-node server supports PostgreSQL v3.0/3.2, TLS, authentication, DDL/DML, transactions and savepoints, row/table locks, full transaction IDs and snapshots, views, materialized views, modeled indexes, sequences, domains, enums, PostgreSQL large objects, full-text search, SQL functions (scalar, `SETOF`, and `TABLE`, including mutable and nested calls), CTEs, joins, windows, COPY, PostgreSQL 18 SQL/JSON and SQL/XML, PostgreSQL 18-interoperable logical-replication publishing and bounded subscription bootstrap/apply, and PostgreSQL catalog introspection used by common clients and dump/restore tools. [The PostgreSQL 18 matrix](docs/postgresql-18-compatibility.md) distinguishes implemented behavior, explicit architecture boundaries, and non-goals.

Modeled btree, hash, BRIN, and GiST indexes are physical access paths. Hash indexes execute
exact single-key probes across plain, expression, partial, prepared, query,
join, and direct UPDATE/DELETE paths; their equality-only contract excludes
ordering, ranges, uniqueness, included columns, and clustering exactly where
PostgreSQL does. BRIN indexes execute equality/range and the built-in
range/network inclusion strategies through bitmap plans over the immutable object-block roster, including
multicolumn, expression, partial, prepared, join, and direct UPDATE/DELETE
paths. Their minmax-multi and Bloom parameters, explicit summarize/desummarize
state, operator-class identities, and relation options retain PostgreSQL 18
catalog, rollback, WAL, checkpoint, ALTER, and cold-recovery behavior. Btree indexes additionally execute composite keys and
equality-leading prefixes with lower, upper, or two-sided bounds on the
following column. Nested loops use those same exact and range probes
when their keys depend on already-bound rows, including multiway joins,
prepared expressions, `UPDATE ... FROM`, and `DELETE ... USING`.
Complete resident equality maps avoid table walks;
durable equality generations carry per-block filters so cold probes skip
unrelated key blocks. Durable keys are checkpoint-sorted with PostgreSQL type
and collation semantics; first/last-key block bounds let prefix and range
probes avoid disjoint object reads. The executor always rechecks MVCC
visibility and the SQL qualification. Compatible `ORDER BY` clauses traverse
those compact keys in declared forward or backward order, including equality-
fixed prefixes and PostgreSQL NULL placement, without materializing or sorting
wide rows. Immutable generations carry versioned `INCLUDE` payloads, so
key-and-payload-covered ordered queries execute as index-only scans after a
cold start and across post-checkpoint updates. Partial indexes own physical
membership-filtered bindings and are selected only when a conservative typed
implication proof establishes that the query entails their predicate. Access
method and operator-class identity survive WAL, checkpoints, copied and
partitioned indexes, reindexing, and object-cold recovery.

GiST indexes physically select exact candidate row identities for supported
network containment, planar-geometric relationships, range and multirange
relationships, and `tsvector`/`tsquery` predicates. The nine PostgreSQL 18
built-in classes, their nine families, 100 strategy rows, 68 support rows,
`tsvector` `siglen`, relation `fillfactor`/`buffering`, included columns, DML
maintenance, WAL, checkpoints, and empty-cache recovery share the same bounded
index state. Geometric predicates navigate object-native bounding-box trees
and skip disjoint subtrees. SQL predicates are evaluated against each selected
encoded key,
so this physical GiST path is exact rather than lossy. Built-in point, box,
polygon, and circle classes also order compact candidates by PostgreSQL's
`<-> point` operator, including parameterized origins, filters, `LIMIT`,
included-column index-only scans, committed overlays, and empty-cache recovery.
PostgreSQL GiST tree-page layout and custom native operator classes are not
implemented and are never advertised as a fallback.

GIN indexes physically execute array containment/overlap, full-text, JSONB
containment/existence, and jsonpath predicates as bitmap plans. SP-GiST
indexes execute network, range, planar-geometric, locale-independent text
ordering, and prefix predicates as index plans. Their 11 PostgreSQL 18
built-in operator classes, 11 families, 90 strategy rows, 56 support rows,
method-specific DDL/options, DML maintenance, WAL, checkpoints, cloning,
partitions, reindexing, and empty-cache recovery share the same bounded index
lifecycle. Built-in point, box, and polygon SP-GiST classes provide the same
`<-> point` K-nearest-neighbor boundary. These paths evaluate immutable encoded
keys exactly. GIN generations extract exact lossy tokens into object-native
posting trees and retain PostgreSQL SQL and MVCC rechecks; they do not claim
PostgreSQL page layout or posting-list representation. None of these paths
claims PostgreSQL SP-GiST node layouts or custom native callbacks. Geometric
GiST/SP-GiST predicates use immutable bounding-box navigation with fixed-startup
construction buffers, bounded traversal, exact rechecks, and covering payloads.
Legacy generations remain readable and are upgraded by the next checkpoint.
The durable node format and memory bounds are documented in
[docs/index-navigation.md](docs/index-navigation.md).
Network, range, full-text, and GIN posting navigation are implemented. Ranked
nearest-neighbor GiST/SP-GiST limits use conservative point-to-box lower bounds
to prune immutable siblings while retaining exact distance and MVCC checks.
Queries with residual filtering, row security, locking, ties, no finite limit,
or an unrankable origin retain the complete exact ordering path.

Catalog object introspection includes PostgreSQL 18 object identification,
descriptions, reversible address records, search-path visibility predicates,
and serial-sequence discovery. These read transaction-visible DDL and retain
their exact OUT-column metadata through scalar, table-function, raw-wire, and
driver boundaries.

Major SQL-object catalogs are independently bounded at startup. `max_tables`
no longer silently sizes indexes, views, materialized views, routines, casts,
operators, operator families/classes, triggers, or publications; each pool has
its own configuration and memory-plan charge.

Database, schema, sequence, and user-defined type catalogs are independently
sized by `max_databases`, `max_schemas`, `max_sequences`, `max_domains`,
`max_enums`, and `max_composites`. Their session state, planner metadata,
catalog rows, database cloning, WAL, checkpoints, and cold recovery use those
declared capacities. Schema slots retain their explicit 255-slot on-disk
representation limit; database slots retain their 65,535-slot limit; sequence
relations retain their disjoint 5,000-slot OID range; and domain, enum, and
named-composite types each retain a disjoint 10,000-slot `pg_type` OID range.
Physical table and view row types likewise reject configurations above their
10,000-slot OID bands instead of synthesizing colliding identities.

User-defined scalar and array types keep their durable schema/name identity
separate from runtime catalog slots. The schema-less spill and constant-default
formats carry full 16-bit array slots, while recovery rebinds domain chains,
enum fields, composite fields, table columns, views, and routines only after
the complete type catalog is present.

Cluster authorization is independently startup-sized through `max_roles`,
`max_role_memberships`, `max_role_settings`, `max_acl_entries`,
`max_column_acl_entries`, `max_default_acl_entries`, and
`max_parameter_acl_entries`. These bounds cover authorization traversal,
connection limits, privilege cascades, catalog rendering, WAL, checkpoints,
and object-cold recovery. Role and privilege slots retain their 65,535-slot
on-disk representation limit; object ACL capacity must reserve the three
built-in public-schema grants.

Legacy configurations that specify only `max_tables` keep the historical
one-slot-per-table defaults for those catalog classes. Row-level security uses
an independent `max_policies` pool (default 256), not eight slots per table;
there is no second per-table policy limit. Policy plans draw from the statement
arena. Routine parameters, output columns, configuration settings, policy role
lists, and trigger arguments accept the parser's complete 64-item boundary.
Trigger arguments follow PostgreSQL's zero-based `TG_ARGV` and NULL-for-none
semantics. Exhaustion is a PostgreSQL program-limit error and
cannot leave a partially published catalog object.

`max_ddl_per_transaction` sizes catalog undo, commit, prepared-transaction,
subscription-apply, and logical-decoding state together. It is reserved for
every transaction slot at startup; exhaustion aborts the current statement or
transaction instead of partially applying bulk DDL. Pending table-definition
and routine-dependency pools use that transaction bound together with
`max_catalog_versions_per_object` (default 8). The latter is an explicit
startup-memory choice rather than a compiled inline-array ceiling and may be
raised when workloads repeatedly change one object in a transaction.

Row MVCC history is independently sized by `max_row_versions_per_row`
(default 8). Startup reserves global pending-command and committed-snapshot
pools from that bound and the configured transaction and row capacities.
Released entries return to constant-time free lists; reaching either the
per-row limit or a global pool reports a named program-limit error before
commit. `max_spill_generations_per_table` (default 8, minimum 2) controls the
immutable-generation fan-out retained for each table. Storage, checkpoint,
merge, temporary-spill, and cold-read rosters all use that single startup
setting; a manifest whose roster exceeds it is rejected rather than truncated.

Atomic transaction bookkeeping is independently startup-sized by
`max_savepoints_per_transaction` (default 16),
`max_deferred_constraints_per_transaction` (128), `deferred_trigger_bytes`
(256KiB), and `max_analyze_per_transaction` (64 statistics changes, including
extended statistics). These capacities cover connection, prepared-transaction,
and subscription-apply slots. Savepoint configuration also reserves GUC,
foreign-query, large-object descriptor depth, and per-relation nested statistics
state; session reset reuses the original allocation. Statement arenas, row undo,
and WAL staging remain separate named bounds. Pending table and extended
statistics pools combine `max_analyze_per_transaction` with
`max_catalog_versions_per_object`, including repeated ANALYZE of one object and
savepoint rollback.

TRUNCATE fan-out uses the complete configured physical-table capacity, including
inheritance and partition descendants and foreign-key cascades. Transaction and
logical-decoding table lists are startup-reserved, not sixteen-table arrays.
The journal writes a 16-bit relation-count record and still reads the older
8-bit count format; pgoutput retains its PostgreSQL 32-bit relation count.

Checkpoint maintenance is normally paced. At critical row-cache pressure it
finishes publication before dispatch resumes, preventing many-table sweeps
from exhausting the cache during small autocommit writes. Oversized pending
transactions still report their named memory bound; cache pressure never
grows a pool or weakens durable publication.

Collations, conversions, text-search objects, event triggers, tablespaces, and
object comments likewise have independent startup capacities. Their catalog
queries allocate from the fixed statement arena according to the actual pool,
and text-search WAL uses a backward-readable 16-bit slot record so identities
above 255 survive journal and object-cold recovery without truncation.

Checkpoint and temporary-spill bookkeeping is sized from `max_tables`, including
the internal large-object relation. Checkpoint publication, compare-and-swap
retry, and empty-cache recovery therefore cover every configured physical table
slot instead of silently stopping at 1,024. `checkpoint_manifest_bytes` reserves
the complete serialized catalog image at startup and reports named exhaustion
before publication. `checkpoint_commit_batches`, `checkpoint_live_blocks`, and
`checkpoint_merge_entries` independently size cold-recovery ordering, the live
block keep-set, and one SST-pair merge. `checkpoint_garbage_batch_objects`
paces deletion without imposing a ceiling on accumulated obsolete objects;
successful explicit checkpoints drain every batch. All four reservations are
charged before serving, and configured exhaustion names the responsible bound.
Table, constraint, default, statistics, publication,
replication, subscription, dependency, trigger, sequence, and information-schema
catalog builders use their transaction-visible cardinality rather than hidden
256/512/1,024-row arrays.

Wide schema objects use one explicit boundary through DDL, enforcement,
catalogs, WAL, checkpoints, and recovery. Index tuples and partition keys match
PostgreSQL 18's 32-attribute limits; the bounded SQL parser admits 64 table
constraints of each modeled kind, 64 domain checks, 64 composite fields, and
64 LIST-bound values. Direct inheritance parents, defaults across every view
column, subscription publication names, event-trigger tags, foreign OPTIONS
clauses, and operator-family operator/support-function catalogs use that same
64-item statement boundary. Crossing a boundary returns a program-limit error
before catalog mutation. Index key/include counts and `pg_partitioned_table`
expose the same accepted shape after empty-cache recovery.

Wide statements likewise use the parser's complete 64-item list boundary for
simple and extended execution: Bind parameters, CTEs, row-locking clauses,
window definitions, `ALTER TABLE` actions, `JOIN ... USING` columns, aggregate
and scalar-subquery scratch, set-operation leaves, and anonymous record shapes
are described, planned, executed, and explained at that width. A window may
compose 64 partition keys with 64 ordering keys. `EXPLAIN` plan nodes live in
the fixed statement arena rather than the worker stack, and over-boundary
input returns SQLSTATE `54000` without dropping parse warnings or partially
executing the statement. The maximum accepted shapes are qualified before and
after empty-cache object recovery.

Implicit index OIDs reserve the complete enforcer stride. Constraint kinds use
disjoint catalog-local OID bands for every accepted table slot and position.
Partition-trigger clone OIDs include the
durable trigger generation and the complete table-slot stride. Creation and
replay reject finite OID-generation exhaustion before installing an object.
These synthesized OID assignments changed with the widened schema format;
upgrading an existing deployment requires logical dump/restore rather than
assuming old persisted `oid`/`regclass` values retain their numeric identity.

SQL/JSON includes first-class `jsonpath`/`jsonpath[]`, strict and lax path execution, path operators and functions, SQL-standard query and construction functions, `JSON_TABLE`, record conversion, SQL/JSON aggregates, and JSONB read/write subscripting. These types and expressions cross text/binary wire, COPY, stored-query, PL/pgSQL, WAL, checkpoint, and object-cold recovery boundaries. [SQL/JSON compatibility and limits](docs/sql-json.md).

SQL/XML includes first-class `xml`/`xml[]`, constructors and predicates,
bounded namespace-aware XPath, ordered `XMLAGG`, typed `XMLTABLE`, and the
session-visible `xmloption` input mode. The same wire, COPY, stored-query,
PL/pgSQL, WAL, checkpoint, and object-cold recovery boundaries apply.
[SQL/XML compatibility and limits](docs/sql-xml.md).

PostgreSQL planar geometry includes all seven native value types, documented
construction and conversion functions, transforms, distance and intersection
operators, spatial relationships, component reads and updates, prepared-query
typing, binary wire values, and object-cold recovery.

PostgreSQL transaction introspection includes `xid8`, `pg_snapshot`, their
historical `txid` aliases, snapshot set-returning functions, current-ID and
status functions, arrays, indexing, text/binary protocol values, and durable
ordinary and prepared-transaction status across object-cold recovery.

PostgreSQL advisory locking includes the complete session and transaction
function families for 64-bit and two-integer keys, shared and exclusive modes,
blocking and try acquisition, reentrant session holds, savepoints, disconnect,
and prepared-transaction recovery. Advisory, row, and relation waits share one
deadlock graph. `pg_backend_pid()`, `pg_blocking_pids()`, and `pg_locks` expose
live granted and waiting locks with PostgreSQL 18 catalog and wire types. The
startup-only `max_locks_per_transaction` setting sizes the fixed lock pool;
exhaustion fails explicitly.

A startup-bounded backend registry drives `pg_stat_activity` and `pg_stat_ssl`,
including live query, transaction, lock-wait, client, application, and
negotiated TLS state. `pg_cancel_backend()` and zero-timeout
`pg_terminate_backend()` signal that same registry; protocol and SQL
cancellation share transaction rollback and extended-protocol synchronization.
`pg_listening_channels()` exposes committed session registrations,
`pg_notification_queue_usage()` reflects the reactor's immediately drained
queue, and `pg_stat_database_conflicts` reports the exact zero-conflict state of
a server that never enters PostgreSQL hot standby.

Startup-budgeted cumulative statistics back PostgreSQL 18's all/system/user
table, transaction-local table, index, and database views. Query, DML, COPY,
MERGE, maintenance, transaction, deadlock, and session choke points update the
same counters; commit, abort, nested savepoint release/rollback, transactional
TRUNCATE, relation/index reuse, ANALYZE, VACUUM, and reset functions retain
PostgreSQL's distinct semantics.
Heap-page, HOT-update, autovacuum, parallel-worker, and standby-conflict fields
remain exact zero states because those PostgreSQL subsystems do not exist in
the object-native engine. Statistics control functions return PostgreSQL's
non-null zero-length `void` value over text and extended protocol paths.

PostgreSQL `money` is an exact signed-cent type with C/en_US monetary text,
scalar and array binary wire formats, casts, comparisons, arithmetic, support
functions, aggregates, btree indexes and catalogs. Values retain that identity
through COPY, stored rows, WAL, checkpoints, and object-cold recovery;
unsupported monetary locales fail explicitly.

PostgreSQL 18 binary and bit strings include the complete modeled `bytea`,
fixed `bit`, and `varbit` scalar function/operator families, checksums,
integer conversions, raw-byte and bitwise aggregates, and direct support
routines. Exact procedure, operator, aggregate, cast, btree, and hash catalogs
agree with PostgreSQL; binary wire/COPY and driver values, generated and check
expressions, indexes, WAL, checkpoints, and object-cold recovery retain the
same bounded byte representation.

PostgreSQL 18 text execution includes the complete core scalar and
set-returning string family, SQL-standard normalization syntax and predicates,
Unicode 16 normalization/assignment/case folding, Unicode escapes,
single-byte `to_ascii` transliteration, and integer binary/octal/hex rendering.
The built-in `pg_unicode_fast` collation selects full Unicode casing while C,
POSIX, and `ucs_basic` retain PostgreSQL's byte-oriented rules. Exact function
and collation catalogs, generated/check expressions, indexes, stored views,
prepared statements, WAL, checkpoints, and object-cold recovery share the same
startup-bounded representation.

PostgreSQL 18 regular expressions include BRE, ERE, literal, and ARE modes,
the complete flag and `regexp_*` overload surface, named arguments, captures
and backreferences, lookaround and word constraints, character classes and
escapes, and the native text/name operators. Exact procedure and operator
catalogs, generated/check expressions, expression indexes, views, prepared
statements, WAL, checkpoints, and object-cold recovery share one bounded
matcher whose complexity exhaustion is an explicit error.

PostgreSQL 18 network addresses include `inet`, `cidr`, `macaddr`, and
`macaddr8` parsing, formatting, inspection, containment, arithmetic, bitwise
operations, hashes, and `inet` extrema. Exact procedure, operator, cast,
aggregate, btree, and hash catalogs share PostgreSQL's prefix-first network
ordering. Binary wire/COPY, arrays, indexes, constraints, generated columns,
views, WAL, checkpoints, and object-cold recovery retain the same bounded
canonical address representation.

PostgreSQL 18 ranges and multiranges include all six built-in subtype
families, cross range/multirange containment and positional operators, direct
comparison/set/hash/canonical/subdiff support routines, union and intersection
aggregates, and multirange `unnest`. Exact polymorphic procedure, operator,
aggregate, btree, and hash catalogs agree with PostgreSQL. Values keep their
type through arrays, binary wire/COPY, rows, indexes, constraints, generated
columns, views, WAL, checkpoints, and object-cold recovery.

PostgreSQL `tid` and `cid` values and arrays retain their exact unsigned tuple
and command identity through SQL, btree/hash support, text and binary wire/COPY,
rows, spills, WAL, checkpoints, and object-cold recovery. Their PostgreSQL 18
catalog OIDs, operators, support routines, aggregates, and operator families
are visible to clients. A local heap `ctid` system column is not synthesized:
object-native row identities are not PostgreSQL heap-version addresses, so a
reference fails explicitly instead of returning a stable but incorrect value.

PostgreSQL `aclitem` values retain grantee and grantor OIDs independently from
their rendered names, so scalar values, arrays, and defaults follow role renames
and fall back to numeric OIDs after role removal across WAL, checkpoints, and
object-cold recovery. ACL input/output, containment, equality, hashes,
`makeaclitem`, `aclexplode`, and `pg_get_acl` share the object-privilege catalog
boundary, including foreign-data wrappers, foreign servers, and large objects.

PostgreSQL `pg_lsn` is a first-class unsigned WAL-position value across scalar
and array SQL, comparisons, numeric arithmetic, hashes, extrema, indexes,
catalogs, text wire/COPY, rows, WAL, checkpoints, and object-cold recovery.
Physical WAL inspection remains an explicit object-native architecture boundary.

PostgreSQL 18 UUIDs include strict flexible-form input, cryptographically
random `gen_random_uuid()`/`uuidv4()`, monotonic sub-millisecond `uuidv7()`
with timezone-aware interval shifts, and version/timestamp extraction for RFC
UUIDs. Function identities and named arguments are catalog-visible, generated
values cross text/binary extended protocol and procedural execution, and UUID
defaults, generated columns, checks, indexes, WAL, checkpoints, and object-cold
recovery retain the same typed value boundary. Session-zone timestamp input,
extraction, truncation, construction, `AT TIME ZONE`, JSON-path comparison, and
calendar interval arithmetic share PostgreSQL's daylight-saving gap and
ambiguity resolution.

PostgreSQL 18 temporal values include date, time, time with time zone,
timestamp, timestamp with time zone, and interval infinities where PostgreSQL
defines them. Their casts, cross-type comparisons, arithmetic, `AT LOCAL`,
zone-explicit `date_add`/`date_subtract` and `date_trunc`, constructors,
extraction, hashes, extrema, interval sum/average, exact catalogs, binary wire,
WAL, checkpoints, and object-cold recovery share one representation. Numeric
positive and negative infinity use PostgreSQL's native numeric binary signs and
remain distinct from `NaN` through storage and arithmetic.

Permanent, unlogged, and session-temporary tables, views, indexes, identity sequences, standalone sequences, CTAS, and `SELECT INTO` have distinct PostgreSQL lifetimes. A view becomes temporary when requested or when any captured relation is temporary, including through another view. Temporary relations use isolated per-connection namespaces and `ON COMMIT` actions and never enter WAL, checkpoints, object storage, template clones, or logical publications. Committed temporary rows spill to a bounded, startup-sized local store (`temporary_spill_bytes`, or `0` to keep them resident-only) that is recreated empty on restart. Unlogged definitions are durable, retain rows after a clean shutdown, and reset table and sequence state after an unclean restart.

Verification includes unit/property tests, SQLLogicTest and differential runs against PostgreSQL, psql and driver probes, object-store cold-start and crash recovery, and deterministic storage fault simulation. A versioned S3 compatibility profile is locked by golden-wire fixtures and required black-box CI against independently implemented compatible endpoints.

All 183 PostgreSQL 18 top-level commands have a tested execution contract or an explicit architecture boundary; `tests/postgresql18_commands.tsv` is the ratchet. Logical replication interoperates with PostgreSQL 18 publishers, subscribers, and `pg_recvlogical` at the object-native boundary, including transactional and nontransactional logical messages, typed slot-management SQL, and replication monitoring views. Continuing differential, driver, and dump/restore testing remains the compatibility-discovery ratchet. Physical demand is proven through query execution and DML sources; PostgreSQL physical/binary-WAL replication is not a target. See [PLAN.md](PLAN.md) and [the logical-replication boundary](docs/logical-replication.md).

## Quick start

```sh
# The development configuration defaults to local-only durability. Its object
# storage section documents the direct S3-compatible settings for durable mode.
cargo run --release -- --config examples/dev.conf
psql -h 127.0.0.1 -p 5433 -U you
```

## Project documents

- [PLAN.md](PLAN.md) — completion roadmap
- [BUGS.md](BUGS.md) — unresolved, genuinely blocked bugs only
- [docs/terminology.md](docs/terminology.md) — naming and glossary
- [docs/object-storage.md](docs/object-storage.md) — direct S3-compatible durability boundary
- [docs/postgresql-18-compatibility.md](docs/postgresql-18-compatibility.md) — implemented PostgreSQL 18 and explicit non-goals
- [docs/performance.md](docs/performance.md) — current single-process and replica scaling boundary
- [docs/logical-replication.md](docs/logical-replication.md) — PostgreSQL 18 protocol, SQL, monitoring, and architecture boundary
- [docs/sql-json.md](docs/sql-json.md) — PostgreSQL 18 SQL/JSON, jsonpath, wire, and durability boundary
- [docs/sql-xml.md](docs/sql-xml.md) — PostgreSQL 18 SQL/XML, XPath, wire, and durability boundary
- [AGENTS.md](AGENTS.md) — contribution rules

## References

- [PostgreSQL frontend/backend protocol](https://www.postgresql.org/docs/current/protocol.html)
- [TigerBeetle safety and design](https://docs.tigerbeetle.com/concepts/safety/)
