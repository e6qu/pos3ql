# pos3ql roadmap

Architecture: [README.md](README.md). Naming: [docs/terminology.md](docs/terminology.md). Working rules: [AGENTS.md](AGENTS.md). Unresolved blockers only: [BUGS.md](BUGS.md).

## Architecture objectives

- PostgreSQL compatibility is SQL text, v3 wire bytes, catalog behavior, and logical replication where it suits the object-native design; PostgreSQL heap pages, physical XLOG, and binary-WAL replication are not targets.
- Object storage is the only durable tier. RAM and local disk are bounded, disposable caches.
- The engine uses one generic object-store gateway contract; provider protocols and SDKs remain outside the application.
- Acknowledgement must have an object-native latency and request shape: immutable commit batches plus CAS publication, not one remote upload per transaction.
- Runtime memory is fixed at startup. Pool exhaustion is a loud error.
- Unsupported PostgreSQL semantics fail explicitly; no fallback or accept-and-ignore path exists.

## Completion work

1. **Object-native durability.** Complete: readable protocol batches publish immutable commit data and a CAS commit head before acknowledgement; checkpoints retain their replay boundary, clear only manifest-captured table generations, and retry post-publication cleanup until completion.
2. **Object-store portability.** Complete: application configuration and transport use only the generic gateway contract; the deterministic simulator and integration gateway qualify that contract. No application provider adapter or provider-specific branch is permitted.
3. **SQL and catalogs.** Close remaining language, type, privilege, collation, and introspection differences through strict PostgreSQL differential tests. Index keys are typed columns or expressions, with `INCLUDE`, partial predicates, and `NULLS NOT DISTINCT` retained across transactions, WAL, checkpoints, catalogs, uniqueness checks, and conflict arbitration. Global, tablespace, and concurrent variants remain explicit gaps. Aggregate window `FILTER` is supported; PostgreSQL rejects `DISTINCT` and argument ordering in a window call.
4. **Wire and binary protocol.** Match PostgreSQL Bind, Result, COPY, cancellation, array/range/composite, and typmod bytes for every accepted type. One catalog-aware binary resolver serves Bind and nested records, including domains, enums, arrays, and unknown text; binary cursors retain binary result framing through FETCH; malformed structured values fail as a whole.
5. **Logical replication.** Keep pgoutput, slot, and subscription work only where it preserves object-native performance; physical/binary-WAL replication remains deliberately unsupported.
6. **Physical-demand execution.** Preserve needed-block/column proofs through joins, correlation, sorting, grouping, windows, spill, and DML source assignments.

## Delivery order

One large PR is open at a time; it is merged and `main` is refreshed before the next begins.

1. **Arrays and composites.** Complete: canonical rectangular array shapes retain bounds through SQL, storage, Bind, Result, and COPY; anonymous records have matching binary receive/send codecs.
2. **SQL semantics.** In progress: generated PostgreSQL-valid SQL has no unsupported budget; differential comparison preserves decoded value types and total result order, while exercising arrays, joins, correlated subqueries, windows, volatile set-operation leaves, and DML target aliases. Array aggregation, ARRAY subqueries, bounds, and extended-protocol result types share one rectangular-shape model. Close the remaining differences through strict differential tests.
3. **DDL and catalogs.** Typed column, domain, enum, sequence, schema, view, routine, and index definitions are transaction-owned state across DDL, WAL, recovery, catalogs, and query description. Index renames, expression keys, predicates, `INCLUDE` columns, and `NULLS NOT DISTINCT` follow one durable path through rollback, checkpoints, conflict handling, and `pg_indexes`; remaining catalog work is broader PostgreSQL coverage. Functions and procedures retain overload identity, owner, ACLs, scalar, set, and table result contracts, and recovery identity; a function body is one typed query, and `RETURNS SETOF` and `RETURNS TABLE` functions are typed table sources. Procedures retain validated statement programs whose internal results never escape `CALL`. Routine query inputs and results cross a bounded projected-row boundary. Synthesized results are statement-arena bounded and fail on exhaustion.
4. **Wire and bulk data.** In progress: typed binary record input shares catalog resolution with Bind and preserves nested domain constraints. Close accepted-type Result, COPY, typmod, portal, and driver behavior.
5. **Replication and physical demand.** In progress: publication creation, ownership, renaming, and transactional table/schema selection retain one committed definition plus typed pending state; pgoutput v1–v4, slot acknowledgement, and raw-wire probes cover the stream boundary. Snapshot export is rejected explicitly rather than returning a false snapshot. Cold PAX scans retain every source column used by `UPDATE ... FROM` assignments. Complete object-native logical replication and demand propagation through every executor path.

## Current invariants

- Catalog identity, DDL visibility, domain, enum, and sequence definitions, and retained TZif cache entries are atomic typed states.
- A sequence's staged parameters and value state are visible only to the owning transaction; rollback restores the exact savepoint image and WAL publishes only the final committed image.
- A slot acknowledgement is validated before its WAL record can be committed; pgoutput versions are parser-validated protocol states.
- DML target aliases bind uniformly in predicates, joined sources, static result description, and `RETURNING` evaluation.
- SQL function query inputs and results cross the projected-row codec; scalar calls cannot resolve set-returning definitions, and table calls cannot resolve scalar definitions. `RETURNS TABLE` owns ordered result names and types through replacement, WAL, checkpoints, catalogs, and scans.
- Publication identity, membership, schema selectors, operations, and ownership are durable typed states; catalog reads see their own staged state, while replication sees only committed selection.
- Engine startup initializes named-zone TZif history before allocation freezes; unavailable historical data is not approximated.
- Checksums, immutable blocks, and CAS manifests make cache loss recoverable.
- Cancellation keys are parsed as protocol-versioned values: v3.0 uses four bytes and v3.2 uses the full issued key.
- Network values are constructed only from a valid address family, mask, and canonical address bytes; `cidr` additionally requires host bits clear.
- Verification includes unit/property tests, PostgreSQL differential and SQLLogicTest corpora, driver probes, cold-start/crash recovery, and deterministic fault simulation.
