# Known bugs

The 2026-09-14 physical-index review found no externally blocked defect.
Expression and partial indexes previously retained catalog, uniqueness, and
recovery semantics but had no physical query path. They now own bounded
resident and durable bindings, share typed query/join/DML/EXPLAIN planning,
and require a conservative partial-predicate implication proof. The review
also fixed pending-CREATE population, expression/partial tuple-size validation,
expression-key INCLUDE payload identity, pending-binding checkpoint eligibility,
cross-database index-binding isolation, per-row parser-scratch retention, and
catalog-free expression evaluation that rejected named-composite fields and
schema-qualified text-search configurations during index population and cold
recovery.

There are no known unresolved bugs. Last reviewed: 2026-09-14 against the direct, provider-neutral S3-compatible object-store boundary and the complete 183-command PostgreSQL 18 inventory, the complete PostgreSQL 18 text and Unicode 16 function/grammar/collation/catalog surface, the complete PostgreSQL 18 mathematical function and overload surface including exact degree trigonometry, special functions, numeric infinities and uniform/normal random distributions, the complete PostgreSQL 18 temporal scalar/operator/cast/aggregate and btree/hash catalog boundary including infinities, cross-type semantics, explicit zones and object-cold recovery, the complete built-in range/multirange operator, support-routine, aggregate, SRF and executable btree/hash catalog boundary, strict UUID input and PostgreSQL 18 UUIDv4/UUIDv7 generation/extraction across catalogs, wire, procedural execution and recovery, exact-cent `money`, PostgreSQL 18 binary and bit string functions/operators/aggregates/casts and btree/hash catalogs across wire, COPY, indexes and object-cold recovery, complete `inet`/`cidr`/`macaddr`/`macaddr8` scalar operations and exact prefix ordering, support routines, extrema, casts and btree/hash catalogs across wire, COPY, indexes and object-cold recovery, low-level `tid`/`cid` scalar and array identities across SQL, catalogs, wire, COPY and object-cold recovery, first-class `aclitem` role identities, functions, SRF, catalogs, persistence, rename/removal behavior and object-privilege overloads, complete `pg_lsn` parsing, numeric arithmetic, comparison, hashing, extrema, catalogs and durability, PostgreSQL 18 `refcursor`/`refcursor[]`, native procedural cursor control, live cursor catalogs and binary/persistence boundaries, PostgreSQL 18 `regcollation`/`regcollation[]`, the complete `to_reg*` lookup family, catalog-reference casts, stable stored dependencies and cold-recovery name refresh, PostgreSQL 18 SQL/JSON and jsonpath, bounded PostgreSQL 18 SQL/XML and XPath, planar geometric functions/operators/subscripts and non-finite wire values, full transaction identities/snapshots/status and prepared-transaction recovery, complete PostgreSQL 18 advisory-lock functions, shared deadlock detection, lock observability and prepared-lock recovery, live PostgreSQL 18 backend activity/TLS monitoring, backend signaling, LISTEN/NOTIFY introspection and zero-conflict standby reporting, foreign-table MERGE, lazy foreign-session savepoint mirroring, maintenance-target inheritance selection, catalog-atomic native-hook rejection, relation persistence and session teardown, transitive temporary-view inference, dependent view-column identity, template-clone exclusion, bounded local temporary spill, exact clean/crash recovery, temporary WAL/checkpoint/publication exclusion, typed foreign-session ownership, outbound Bind framing, routine and PL/pgSQL execution, roles and privileges, operators and extensions, PostgreSQL 18.6 publisher/subscriber and `pg_recvlogical` interoperability, logical bootstrap catalog introspection, transactional and nontransactional logical messages, typed slot-management and monitoring surfaces, strict pgoutput tuple widths and framing, wire drivers, object-store recovery, and pg_dump/restore. Unsupported PostgreSQL behavior is an explicit typed boundary, not deferred work. Details belong in tests and git history, not this blocker register.

The review also covers the complete bounded PostgreSQL 18 BRE, ERE, literal,
and ARE function/operator/catalog surface and PostgreSQL's upstream regular-
expression regression cases across SQL, stored expressions, and recovery.

The review additionally covers PostgreSQL 18 object identification,
descriptions, reversible addresses, catalog visibility predicates, and
serial-sequence discovery across transaction-local DDL, wire metadata, driver
adaptation, and object-cold recovery. The discovered successive-ALTER sequence
binding, user-type visibility, cascading statistics-drop ownership, cascade
notice, and small-integer extended-protocol coercion defects are fixed in the
same change.

The review additionally covers PostgreSQL 18 cumulative database, table,
transaction-local table, and index statistics across query, DML, COPY, MERGE,
maintenance, commit, abort, transactional TRUNCATE, relation reuse, reset,
catalog, text-wire, and extended-protocol boundaries. The discovered aborted
insert dead-tuple, top-level and nested-savepoint TRUNCATE counter, built-in
initial-privilege, NULL-encoded `void`, omitted database-level index-tuple
return, and NULL-predicate phantom-scan defects are fixed in the same change.

The review additionally covers PostgreSQL 18 cumulative and transaction-local
user-function timing, `track_functions`, statistics reset controls, and the
remaining `pg_statio_*`, SLRU, WAL receiver, recovery-prefetch, GSSAPI,
archiver, background-writer, checkpointer, I/O, WAL, and command-progress
catalogs. The discovered foreign/partitioned-parent statistics leakage, failed
function-call accounting, nullable shared-reset behavior, and fixed
`pg_attribute` row ceiling are fixed in the same change.

The review additionally covers PostgreSQL 18 system-relation identity and
attribute metadata, prepared-statement descriptors, compatibility role views,
replication origins, wait events, extended-statistics views, and installed
time-zone catalogs. The discovered catalog OID/width drift, duplicate
large-object rows, catalog pseudo-type persistence-code collision, incorrect
`typbyval` inference, non-TZif metadata ingestion, truncated-MCV base-frequency
calculation, allocating MCV tie sort, and replication-origin commit-LSN replay
defects are fixed in the same change. Expanding the catalog exposed an undersized
default SQL arena that rejected ordinary pgJDBC and pgx introspection; the
startup-fixed default now covers that complete working set. Exact MCV base
frequencies and subscription origin positions are covered through WAL,
checkpoint, and cold recovery.

Record only a genuinely intractable or externally blocked defect here. A row must include a stable ID, a reproducer, and the reason it cannot be fixed now. Fixable work belongs in the same change that finds it; fixed-bug history belongs in git history and pull requests.

| ID | Status | Found | Description | Repro | Blocker |
|----|--------|-------|-------------|-------|---------|

The PostgreSQL 18.6 vendored regression expansion found no externally blocked
defect. Float4 enum-order exhaustion, uncommitted enum-value safety, aliasless
derived tables, bit/character/type-input edges, and truncated enum diagnostics
from `pg_input_error_info` were fixed in the same change. Remaining physical
index methods, planner parity, and native server extensions are architecture limits
recorded in the compatibility and performance plans, not deferred bugs.

The performance and scaling review found that each readable connection owned
its own synchronous object-publication barrier and that a statement resumed
after a row-lock or object-read wait could flush success without any object
publication barrier. Both paths now enter one fixed-capacity reactor-wide
response batch, and the CI performance smoke test ratchets its durability,
memory, cold-recovery, and request-amplification behavior. External driver
testing then exposed deferred session teardown allowing another same-turn
connection to observe a terminated session's temporary relation; close/EOF
teardown now remains immediate while live responses retain group publication.

The physical-index review found that execution recognized only one-column
literal predicates, `EXPLAIN` maintained a different decision path, resident
exact probes could lose a cost tie to a full scan, and durable equality probes
read every value-index data block. It also found that one transaction's pending
row unnecessarily disabled committed index access for every observer. One
typed leading-key plan now covers
execution and explanation for exact/composite/parameter/prefix/range cases;
resident equality and filtered durable generations close the point-read path,
and pending-row visibility now preserves observer index access without hiding a
writer's own unindexed key. The performance harness now gates the actual scan
counters for reads and synchronized updates, and a synchronized worker failure
aborts its peer barrier instead of hanging the run. No
externally blocked defect remains from this review.

The ordered-index review found that prefix and range plans advertised physical
index access but read every immutable value-index data block, and that
checkpoint insertion order could not support a real seek. Durable generations
are now externally sorted with the indexed PostgreSQL types and collations;
their versioned roster entries carry conservative first/last-key bounds, while
legacy generations remain readable until rewritten. Range execution uses those
bounds to skip disjoint object blocks and still rechecks MVCC and the complete
SQL predicate. Oversized boundary keys conservatively lose pruning rather than
losing rows, malformed bounds reject as corruption, and the performance suite
now exercises indexed tail ranges. The same typed access plan retains both
lower and upper bounds—including prepared and reverse-spelled comparisons—and
`EXPLAIN` now prices the executor's candidate set while retaining joint and
non-index predicate statistics for emitted rows. Exhaustive testing also found
that a dedicated checkpoint sorter exceeded existing construction-stack and
fixed-memory envelopes and that NULL-bearing index keys had no total physical
ordering; checkpoints now lease the startup-owned external-run pool and
durable generations use an explicit internal NULLS FIRST order. The performance smoke then reached a true
cold seek and exposed value-index `NotReady` as a client-visible object error;
all durable value reads now route that state through the reactor's internal I/O
wait boundary. Independent-object-store CI also exposed a redundant full-key
writer buffer exceeding the established 512 MiB qualification envelope; the
pending block now doubles as the ordering cursor, preserving the fixed budget
without weakening the test. No externally blocked defect remains from this
review.

The ORDER BY execution review found that materialization always sorted wide
rows even when one durable btree supplied the exact requested order. The
shared typed plan now validates forward/backward direction, NULL placement,
equality-fixed prefixes, aliases, and ordinals; execution orders compact keys,
streams row identities through `LIMIT`, and decodes key-covered projections
without a base-tuple fetch. Cold testing exposed two request-amplification
bugs: ordered candidate validation point-read every immutable row, and the
resident overlay re-encoded rows already captured by the published index
generation. Immutable commit LSNs are now checked only against newer resident
changes, and only those newer changes enter the overlay. A partial resident
hash cache is no longer visited before its authoritative durable generation.
The review also found that an explicitly different index collation could share
a table-column value binding whose hash and comparison contract it did not
own; such indexes now remain outside that physical binding.
Removing redundant partial-cache probes exposed a reentrant block-stack borrow
when durable uniqueness candidates point-read their rows inside the index
reader callback. Durable probes now expose their encoded key to uniqueness,
which compares it directly without a nested object read or an artificial
limit on the number of durable hash matches.
Coverage-instrumented driver testing also exposed an administrative-termination
race: closing a socket with a concurrently queued frontend message could reset
the connection and discard its already-written `57P01` FatalResponse. Terminating
connections now flush the complete plaintext or TLS transport output, half-close
their write side, and drain raced input within a fixed deadline before release.
The same tests cover stale-key replacement after committed update/delete,
mixed directions, NULLs, and exact index statistics. No externally blocked
defect remains from this review.

The join-index review found that the typed B-tree plan was hard-coded to the
first source and evaluated every key against a no-column context. Consequently,
indexed inner relations were repeatedly scanned, hash joins masked the missing
plan for some equalities, joined UPDATE/DELETE rescanned their source for every
target row, and joined DML `EXPLAIN` omitted the source tree altogether. One
bound-row plan now selects exact, composite, and range keys only from sources
already available at that nested-loop depth; execution evaluates those keys
through the same chained lookup used by predicate rechecks and recycles each
candidate set inside fixed statement memory. The plan applies to inner, cross,
left, multiway, prepared, `UPDATE ... FROM`, and `DELETE ... USING` paths,
while right/full joins, row security, sampling, stale snapshots, unsupported
collation derivations, expression indexes, and partial indexes retain their
authoritative scan paths. The review also found two unsafe classifications:
an inner-table constant equality could lose to the equivalent bound-row
equality and reread one immutable index for every outer row, while a joined-DML
operand mixing the outer target with an unbound inner column could be mistaken
for a complete probe key. Statement-invariant keys now stay on the
once-per-statement hash/scan path, and column-reference classification rejects
partly unbound probes before execution. `EXPLAIN`, physical statistics,
object-cold recovery, committed overlays, bounded-memory stress, and the
performance smoke suite now exercise the same decision. No externally blocked
defect remains from this review.

The covering-index review found that `INCLUDE` columns survived catalogs and
recovery but were absent from physical value-index generations, forcing base
tuple reads, and that exact ordered plans could describe coverage without
retaining durable key bytes. The versioned generation now stores a separately
framed included-column payload, the manifest binds its exact column mask, and
the committed overlay uses the same encoding. Execution and `EXPLAIN` share
the resulting coverage proof for exact and range ordered scans, while stale
durable payloads are rejected by commit LSN before decoding. Full physical
tuple limits are enforced before row and CREATE INDEX commit, including the
union used by redundant compatible indexes. No externally blocked defect
remains from this review.

The hash-index review completed PostgreSQL 18's equality-only physical hash
boundary across DDL, catalogs, planning, execution, DML, joins, WAL,
checkpoints, copied and partitioned indexes, reindexing, statistics, and
object-cold recovery. Full PostgreSQL 18 catalog differentials cover all hash
operator classes, families, operators, and support procedures. The review also
found and fixed JSONB equality against unknown literals, colliding generated
names when `LIKE INCLUDING INDEXES` copied column and expression indexes, and
an incorrect default hash-opclass claim for `refcursor`. No externally blocked
defect remains from this review.

The BRIN review found that the access method existed only as a static
`pg_am` row and could not own an index definition or physical plan. BRIN now
has a method-isolated typed operator-class boundary, exact PostgreSQL 18.6
operator-class/family/strategy/support catalogs, validated relation options,
bitmap query/join/DML plans, durable block-bound pruning, WAL/checkpoint state,
and object-cold recovery. The review also found that user-defined B-tree
default operator classes were eligible while resolving other access methods,
that new index-definition fields could outrun the WAL size invariant, that
two-sided range selectivity was costed as a one-sided bound, that bitmap paths
inside DML and joins could be flattened into false ordinary index scans, and
that BRIN could be mislabeled as an ordered or resident-hash scan. Oracle
differential testing then found that a fresh, not-yet-checkpointed BRIN could
select an unavailable durable range reader and falsely return no rows; all
ordered-range plans now require a complete readable generation, and every
candidate builder declines late incompleteness instead of converting it into
an empty result. The same differential found incorrect zero `opckeytype`
values for built-in BRIN classes. The shared index-option parser also accepted
fewer PostgreSQL boolean spellings than the server. These bug classes are
fixed at their typed choke points. No externally blocked defect remains from
this review.
