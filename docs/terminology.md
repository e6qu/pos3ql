# Terminology and naming

## Naming

- Spell identifiers out: `interval`, `buffer`, `expression`, `statement`, and `index`, not abbreviations.
- Standard acronyms are allowed: SQL, WAL, SST, LSN, OID, S3, HTTP, TLS, UUID, JSON, AWS, and VSR.
- Established module names are allowed: `ast`, `eval`, `exec`, `guc`, `io`, `mem`, `pg`, `sim`, `sql`, `vsr`, and `wal`.
- Use a coined term only when it carries a repeated, specific meaning; define it here.
- Single-letter names are limited to conventional local indices and published algorithm notation.

## Modules

- `config`: configuration and memory budget.
- `mem`: fixed-capacity memory, pools, arenas, and allocation guard.
- `io`: operating-system and simulated I/O.
- `pg`: PostgreSQL wire protocol.
- `sql`: parser, executor, query engine, and catalogs.
- `storage`: resident catalog, row state, and visibility.
- `wal`: durable journal encoding and replay.
- `checkpoint`: SST publication and cold recovery.
- `store`: block formats and cache tiers.
- `s3`: S3-compatible object-store adapter.
- `entity tag`: an opaque, strong, quoted object generation token used for compare-and-swap.
- `vsr`: Viewstamped Replication.
- `sim`: deterministic fault simulation.

## Glossary

- **block**: fixed-size, checksummed, content-addressed storage unit.
- **block store**: provider-neutral interface over object storage and cache tiers.
- **checkpoint**: immutable SST publication through a compare-and-swap manifest.
- **cold start**: recovery with RAM and local disk caches absent.
- **declared type identity**: the schema-qualified type visible in catalog, parameter, and replication metadata; distinct from an executor value type.
- **commit batch**: immutable journal bytes plus a descriptor; recoverable only after the CAS commit head names it.
- **durable mode**: `object_store = on`; acknowledgement requires commit-batch publication.
- **manifest**: compare-and-swap root naming the current immutable storage state.
- **MVCC**: visibility by transaction and commit LSN.
- **PAX**: column-oriented row groups inside an SST, allowing selective column reads.
- **physical-demand proof**: the columns a query path may read from a physical row.
- **SST**: immutable sorted table of versioned rows, index, filter, and roster blocks.
- **VOPR**: deterministic simulation that injects faults from a reproducible seed.
- **VSR**: Viewstamped Replication, the consensus protocol used by pos3ql.
- **WAL**: checksummed local journal encoding used for recovery and logical replication; it is not PostgreSQL physical XLOG.
