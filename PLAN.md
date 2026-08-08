# pos3ql roadmap

Architecture: [README.md](README.md). Naming: [docs/terminology.md](docs/terminology.md). Working rules: [AGENTS.md](AGENTS.md). Unresolved blockers only: [BUGS.md](BUGS.md).

## Architecture objectives

- PostgreSQL compatibility is the SQL, v3 wire, binary-format, catalog, and logical-replication boundary; PostgreSQL heap pages and physical XLOG are not.
- Object storage is the only durable tier. RAM and local disk are bounded, disposable caches.
- The engine uses one generic object-store gateway contract; provider protocols and SDKs remain outside the application.
- Acknowledgement must have an object-native latency and request shape: immutable commit batches plus CAS publication, not one remote upload per transaction.
- Runtime memory is fixed at startup. Pool exhaustion is a loud error.
- Unsupported PostgreSQL semantics fail explicitly; no fallback or accept-and-ignore path exists.

## Completion work

1. **Object-native durability.** Complete: readable protocol batches publish immutable commit data and a CAS commit head before acknowledgement; checkpoints retain a separate CAS snapshot root and compact obsolete batch/descriptor pairs without skipping the retained replay boundary.
2. **Object-store portability.** Complete: application configuration and transport use only the generic gateway contract; the deterministic simulator and integration gateway qualify that contract. No application provider adapter or provider-specific branch is permitted.
3. **SQL and catalogs.** Close remaining language, type, privilege, collation, and introspection differences through strict PostgreSQL differential tests.
4. **Wire and binary protocol.** Match PostgreSQL Bind, Result, COPY, cancellation, array/range/composite, and typmod bytes for every accepted type.
5. **Logical replication.** Complete pgoutput versions/messages, slot semantics, and logical subscription/apply for PostgreSQL migration.
6. **Physical-demand execution.** Preserve needed-block/column proofs through joins, correlation, sorting, grouping, windows, and spill.

## Current invariants

- Catalog identity and DDL visibility are atomic typed states.
- Checksums, immutable blocks, and CAS manifests make cache loss recoverable.
- Cancellation keys are parsed as protocol-versioned values: v3.0 uses four bytes and v3.2 uses the full issued key.
- Verification includes unit/property tests, PostgreSQL differential and SQLLogicTest corpora, driver probes, cold-start/crash recovery, and deterministic fault simulation.
