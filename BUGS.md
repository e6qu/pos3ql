# Known bugs

There are no known unresolved bugs. Last reviewed: 2026-09-12 against the direct, provider-neutral S3-compatible object-store boundary and the complete 183-command PostgreSQL 18 inventory, the complete PostgreSQL 18 text and Unicode 16 function/grammar/collation/catalog surface, the complete PostgreSQL 18 mathematical function and overload surface including exact degree trigonometry, special functions, numeric infinities and uniform/normal random distributions, the complete PostgreSQL 18 temporal scalar/operator/cast/aggregate and btree/hash catalog boundary including infinities, cross-type semantics, explicit zones and object-cold recovery, the complete built-in range/multirange operator, support-routine, aggregate, SRF and executable btree/hash catalog boundary, strict UUID input and PostgreSQL 18 UUIDv4/UUIDv7 generation/extraction across catalogs, wire, procedural execution and recovery, exact-cent `money`, PostgreSQL 18 binary and bit string functions/operators/aggregates/casts and btree/hash catalogs across wire, COPY, indexes and object-cold recovery, complete `inet`/`cidr`/`macaddr`/`macaddr8` scalar operations and exact prefix ordering, support routines, extrema, casts and btree/hash catalogs across wire, COPY, indexes and object-cold recovery, low-level `tid`/`cid` scalar and array identities across SQL, catalogs, wire, COPY and object-cold recovery, first-class `aclitem` role identities, functions, SRF, catalogs, persistence, rename/removal behavior and object-privilege overloads, complete `pg_lsn` parsing, numeric arithmetic, comparison, hashing, extrema, catalogs and durability, PostgreSQL 18 `refcursor`/`refcursor[]`, native procedural cursor control, live cursor catalogs and binary/persistence boundaries, PostgreSQL 18 `regcollation`/`regcollation[]`, the complete `to_reg*` lookup family, catalog-reference casts, stable stored dependencies and cold-recovery name refresh, PostgreSQL 18 SQL/JSON and jsonpath, bounded PostgreSQL 18 SQL/XML and XPath, planar geometric functions/operators/subscripts and non-finite wire values, full transaction identities/snapshots/status and prepared-transaction recovery, complete PostgreSQL 18 advisory-lock functions, shared deadlock detection, lock observability and prepared-lock recovery, live PostgreSQL 18 backend activity/TLS monitoring, backend signaling, LISTEN/NOTIFY introspection and zero-conflict standby reporting, foreign-table MERGE, lazy foreign-session savepoint mirroring, maintenance-target inheritance selection, catalog-atomic native-hook rejection, relation persistence and session teardown, transitive temporary-view inference, dependent view-column identity, template-clone exclusion, bounded local temporary spill, exact clean/crash recovery, temporary WAL/checkpoint/publication exclusion, typed foreign-session ownership, outbound Bind framing, routine and PL/pgSQL execution, roles and privileges, operators and extensions, PostgreSQL 18.6 publisher/subscriber and `pg_recvlogical` interoperability, logical bootstrap catalog introspection, transactional and nontransactional logical messages, typed slot-management and monitoring surfaces, strict pgoutput tuple widths and framing, wire drivers, object-store recovery, and pg_dump/restore. Unsupported PostgreSQL behavior is an explicit typed boundary, not deferred work. Details belong in tests and git history, not this blocker register.

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
from `pg_input_error_info` were fixed in the same change. Unsupported hash-index execution,
physical planner parity, and native server extensions are architecture limits
recorded in the compatibility and performance plans, not deferred bugs.
