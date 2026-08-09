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
3. **SQL and catalogs.** Close remaining language, type, privilege, collation, and introspection differences through strict PostgreSQL differential tests. Aggregate window `FILTER` is supported; PostgreSQL rejects `DISTINCT` and argument ordering in a window call.
4. **Wire and binary protocol.** Match PostgreSQL Bind, Result, COPY, cancellation, array/range/composite, and typmod bytes for every accepted type. One catalog-aware binary resolver serves Bind and nested records, including domains, enums, arrays, and unknown text; binary cursors retain binary result framing through FETCH; malformed structured values fail as a whole.
5. **Logical replication.** Keep pgoutput, slot, and subscription work only where it preserves object-native performance; physical/binary-WAL replication remains deliberately unsupported.
6. **Physical-demand execution.** Preserve needed-block/column proofs through joins, correlation, sorting, grouping, windows, and spill.

## Delivery order

One large PR is open at a time; it is merged and `main` is refreshed before the next begins.

1. **Arrays and composites.** Complete: canonical rectangular array shapes retain bounds through SQL, storage, Bind, Result, and COPY; anonymous records have matching binary receive/send codecs.
2. **SQL semantics.** In progress: generated PostgreSQL-valid SQL has no unsupported budget; array aggregation, ARRAY subqueries, bounds, and extended-protocol result types share one rectangular-shape model. Close the remaining differences through strict differential tests.
3. **DDL and catalogs.** In progress: bytewise collations; typed constraint metadata; ACL-derived relation and usage grants; and sequence metadata match PostgreSQL. Column defaults are one typed state across DDL, WAL, checkpoints, catalog rendering, and referential actions; expression defaults run for every `SET DEFAULT` action. Login verification has explicit bootstrap, stored-role, and rejection states. Finish domains and remaining client-tool introspection.
4. **Wire and bulk data.** In progress: typed binary record input shares catalog resolution with Bind and preserves nested domain constraints. Close accepted-type Result, COPY, typmod, portal, and driver behavior.
5. **Replication and physical demand.** In progress: publication DDL, monotonic slot acknowledgement, and pgoutput v1–v4 default streams are covered by unit and raw-wire probes. Snapshot export is rejected explicitly rather than returning a false snapshot. Complete object-native logical replication and demand propagation through every executor path.

## Current invariants

- Catalog identity, DDL visibility, and retained TZif cache entries are atomic typed states.
- A slot acknowledgement is validated before its WAL record can be committed; pgoutput versions are parser-validated protocol states.
- Checksums, immutable blocks, and CAS manifests make cache loss recoverable.
- Cancellation keys are parsed as protocol-versioned values: v3.0 uses four bytes and v3.2 uses the full issued key.
- Network values are constructed only from a valid address family, mask, and canonical address bytes; `cidr` additionally requires host bits clear.
- Verification includes unit/property tests, PostgreSQL differential and SQLLogicTest corpora, driver probes, cold-start/crash recovery, and deterministic fault simulation.
