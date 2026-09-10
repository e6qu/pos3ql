# pos3ql

pos3ql is a PostgreSQL-compatible database engine in Rust. SQL and the PostgreSQL v3 simple and extended protocols are the compatibility boundary. Durable data lives in provider-neutral object storage; RAM and local disk are bounded caches.

## Architecture

- PostgreSQL clients: psql, JDBC, Npgsql, psycopg, node-postgres, and pgx use the ordinary wire protocol, including catalog-typed text/binary Bind, Result, COPY, and set-returning integer/numeric/temporal output for implemented types.
- Durable state: immutable commit batches, immutable SST blocks, and compare-and-swap roots. A node can cold-start with an empty local disk.
- Object storage: the engine depends only on a generic gateway with immutable or conditional PUT, full/ranged GET, LIST, DELETE, and strong-ETag compare-and-swap. Provider protocols and SDKs are outside the application. [Contract and qualification](docs/object-storage.md).
- Memory: all runtime memory is budgeted at startup. Pools and queues have fixed limits; exhaustion is an error.
- Determinism: the core is event-driven and runs under deterministic fault simulation.

## Durability

| Mode | Acknowledgement | Survives local-disk loss |
|---|---|---|
| `object_store = off` | local journal sync | no |
| `object_store = on` | immutable commit batch PUT and commit-head CAS | yes |

With object storage enabled, the server groups transactions received in one readable protocol batch, publishes their immutable journal bytes, then advances a CAS commit head before releasing success responses. Checkpoints publish immutable table state through a separate CAS manifest. Recovery follows the commit head beyond that manifest; local disk is a cache.

## Status

The single-node server supports PostgreSQL v3.0/3.2, TLS, authentication, DDL/DML, transactions and savepoints, row/table locks, full transaction IDs and snapshots, views, materialized views, indexes, sequences, domains, enums, PostgreSQL large objects, full-text search, SQL functions (scalar, `SETOF`, and `TABLE`, including mutable and nested calls), CTEs, joins, windows, COPY, PostgreSQL 18 SQL/JSON and SQL/XML, PostgreSQL 18-interoperable logical-replication publishing and bounded subscription bootstrap/apply, and PostgreSQL catalog introspection used by common clients and dump/restore tools.

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

Permanent, unlogged, and session-temporary tables, views, indexes, identity sequences, standalone sequences, CTAS, and `SELECT INTO` have distinct PostgreSQL lifetimes. A view becomes temporary when requested or when any captured relation is temporary, including through another view. Temporary relations use isolated per-connection namespaces and `ON COMMIT` actions and never enter WAL, checkpoints, object storage, template clones, or logical publications. Committed temporary rows spill to a bounded, startup-sized local store (`temporary_spill_bytes`, or `0` to keep them resident-only) that is recreated empty on restart. Unlogged definitions are durable, retain rows after a clean shutdown, and reset table and sequence state after an unclean restart.

Verification includes unit/property tests, SQLLogicTest and differential runs against PostgreSQL, psql and driver probes, object-store cold-start and crash recovery, and deterministic storage fault simulation.

All 183 PostgreSQL 18 top-level commands have a tested execution contract or an explicit architecture boundary; `tests/postgresql18_commands.tsv` is the ratchet. Logical replication interoperates with PostgreSQL 18 publishers, subscribers, and `pg_recvlogical` at the object-native boundary, including transactional and nontransactional logical messages, typed slot-management SQL, and replication monitoring views. Continuing differential, driver, and dump/restore testing remains the compatibility-discovery ratchet. Physical demand is proven through query execution and DML sources; PostgreSQL physical/binary-WAL replication is not a target. See [PLAN.md](PLAN.md) and [the logical-replication boundary](docs/logical-replication.md).

## Quick start

```sh
# Start any implementation of docs/object-storage.md's gateway contract.
cargo run --release -- --config examples/dev.conf
psql -h 127.0.0.1 -p 5433 -U you
```

## Project documents

- [PLAN.md](PLAN.md) — completion roadmap
- [BUGS.md](BUGS.md) — unresolved, genuinely blocked bugs only
- [docs/terminology.md](docs/terminology.md) — naming and glossary
- [docs/object-storage.md](docs/object-storage.md) — portable durability contract
- [docs/logical-replication.md](docs/logical-replication.md) — PostgreSQL 18 protocol, SQL, monitoring, and architecture boundary
- [docs/sql-json.md](docs/sql-json.md) — PostgreSQL 18 SQL/JSON, jsonpath, wire, and durability boundary
- [docs/sql-xml.md](docs/sql-xml.md) — PostgreSQL 18 SQL/XML, XPath, wire, and durability boundary
- [AGENTS.md](AGENTS.md) — contribution rules

## References

- [PostgreSQL frontend/backend protocol](https://www.postgresql.org/docs/current/protocol.html)
- [TigerBeetle safety and design](https://docs.tigerbeetle.com/concepts/safety/)
