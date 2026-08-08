# pos3ql roadmap

Architecture: [README.md](README.md). Naming: [docs/terminology.md](docs/terminology.md). Working rules: [AGENTS.md](AGENTS.md). Unresolved blockers only: [BUGS.md](BUGS.md).

## Architecture objectives

- PostgreSQL compatibility is the SQL, v3 wire, binary-format, catalog, and logical-replication boundary; PostgreSQL heap pages and physical XLOG are not.
- Object storage is the only durable tier. RAM and local disk are bounded, disposable caches.
- The engine uses only immutable or conditional PUT, ranged GET, LIST, DELETE, and ETag CAS. Provider adapters stay below this contract.
- Acknowledgement must have an object-native latency and request shape: immutable commit batches plus CAS publication, not one remote upload per transaction.
- Runtime memory is fixed at startup. Pool exhaustion is a loud error.
- Unsupported PostgreSQL semantics fail explicitly; no fallback or accept-and-ignore path exists.

## Completion work

1. **Object-native durability.** Complete: readable protocol batches publish immutable commit data and a CAS commit head before acknowledgement; checkpoints retain a separate CAS snapshot root. Next, compact commit-head history without weakening logical replay.
2. **Object-store portability.** Shared contract, parsed authorities, typed strong ETags/ranges, and a gateway qualification suite are complete. Native GCS/Azure adapters remain an optimization, not a durability prerequisite.
3. **SQL and catalogs.** Close remaining language, type, privilege, collation, and introspection differences through strict PostgreSQL differential tests.
4. **Binary protocol.** Match PostgreSQL Bind, Result, COPY, array/range/composite, and typmod bytes for every accepted type.
5. **Logical replication.** Complete pgoutput versions/messages, slot semantics, and logical subscription/apply for PostgreSQL migration.
6. **Physical-demand execution.** Preserve needed-block/column proofs through joins, correlation, sorting, grouping, windows, and spill.
7. **Availability.** Connect VSR routing, quorum commit, membership, and failover to object-store publication.

## Current invariants

- Catalog identity and DDL visibility are atomic typed states.
- Checksums, immutable blocks, and CAS manifests make cache loss recoverable.
- Verification includes unit/property tests, PostgreSQL differential and SQLLogicTest corpora, driver probes, cold-start/crash recovery, and deterministic fault simulation.
