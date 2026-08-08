# pos3ql roadmap

Architecture and quick start: [README.md](README.md). Naming: [docs/terminology.md](docs/terminology.md). Rules for changes: [AGENTS.md](AGENTS.md). Known unresolved bugs: [BUGS.md](BUGS.md).

## End state

pos3ql is PostgreSQL-compatible at the SQL and v3 wire boundaries. Object storage is the only durable tier; RAM and local disk are bounded, disposable caches. The storage engine uses one provider-neutral contract: conditional or immutable PUT, full/ranged GET, LIST, DELETE, and ETag compare-and-swap. Provider adapters belong below that contract.

pos3ql WAL is its own checksummed, LSN-ordered recovery log. PostgreSQL physical XLOG compatibility is not a goal: it would require PostgreSQL heap pages and defeat the object-native design. PostgreSQL compatibility for change data is logical replication: pgoutput publishing, then subscription for migration.

## Completion criteria

| Surface | Required state | Current work |
|---|---|---|
| Durable storage | Object-store WAL and manifests recover every acknowledged transaction with RAM and disk caches removed. | Qualify every adapter against the common contract; never add provider-specific engine paths. |
| SQL and catalogs | Supported PostgreSQL behavior matches vanilla PostgreSQL; unsupported behavior is rejected explicitly. | Expand strict differential and dump/restore coverage until the intended language, catalog, locking, and tooling surface is covered. |
| Binary protocol | Bind, Result, and COPY binary format are exact for every implemented type. | Extend with new types, including named composites once they are storable catalog objects. |
| Logical replication | pgoutput correctly publishes retained WAL, relations, types, tuples, truncate, and slot progress. | Add later protocol versions, DDL messages, subscriber migration, and a vanilla-PostgreSQL replication corpus. |
| Object-store execution | Reads fetch only needed verified blocks and columns; materialization spills through the block store. | Preserve physical-demand proofs through every multi-table, correlated, and deferred operator. |
| Availability | Consensus preserves ordering and durability under faults. | Connect the server to live VSR routing, quorum failover, and group commit without weakening bucket-durable acknowledgement. |

## Current invariants

- Static memory: all runtime memory is budgeted at startup; exhaustion is an error.
- Type boundaries: executor value representation, result metadata, and declared catalog type identity are distinct contracts. Durable user-type and parent-domain identities are atomic and must agree with runtime slots.
- Catalog DDL visibility for publications, materialized views, views, schemas, and sequences is one explicit state, including create-then-drop savepoint recovery.
- No silent fallback or no-op: missing data, unsupported semantics, and capacity limits fail loudly.
- Verification: unit/property tests, external PostgreSQL differential tests, SQLLogicTest, psql/driver probes, crash recovery, cold-cache recovery, and deterministic fault simulation are required gates.

## Ordered work

1. Complete physical-demand propagation through query execution.
2. Build and run the logical-replication interoperability corpus.
3. Close remaining SQL, catalog, binary-format, and tooling differences through strict differential tests.
4. Productionize VSR server routing and failover.
5. Continuously qualify object-store adapters against the common contract.
