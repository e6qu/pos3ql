# Terminology and naming

## Naming

- Spell identifiers out: `interval`, `buffer`, `expression`, `statement`, and `index`, not abbreviations.
- Standard acronyms are allowed: SQL, WAL, SST, LSN, OID, HTTP, TLS, UUID, and JSON.
- Established module names are allowed: `ast`, `eval`, `exec`, `guc`, `io`, `mem`, `pg`, `sim`, `sql`, and `wal`.
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
- `sim`: deterministic fault simulation.

## Glossary

Use ordinary words in user guidance and explain a technical term at first use.
For example, write “keep the order version that this transaction read” before
describing snapshot retention. The definitions below make technical details
consistent; readers should not need to memorize them to use the database.

- **block**: fixed-size, checksummed, content-addressed storage unit.
- **block store**: provider-neutral internal interface over object storage and cache tiers; it is not a network protocol or deployable storage service.
- **backup**: a named, checkpoint-consistent manifest and commit-head image whose reachable blocks and commit history are retained until deletion.
- **backup export**: a restartable copy of one named backup and its durable object graph into an empty independently configured object-store prefix.
- **checkpoint**: a saved organization of table data and object references that reduces recovery work. Later commits are recovered from the commit history.
- **compare-and-swap (CAS)**: update a saved reference only if it still has the generation previously read. This prevents an old writer from replacing newer state.
- **cold start**: recovery with RAM and local disk caches absent.
- **declared type identity**: the schema-qualified type visible in catalog, parameter, and replication metadata; distinct from an executor value type.
- **commit batch**: immutable journal bytes plus a descriptor; recoverable only after the CAS commit head names it.
- **durable mode**: `object_store = on`; acknowledgement requires commit-batch publication.
- **entity tag**: an opaque, strong, quoted object generation token used for compare-and-swap.
- **manifest**: the durable inventory of table definitions and immutable storage objects for a checkpoint; updated by compare-and-swap.
- **MVCC**: keeping row versions so transactions see the versions allowed by their isolation level. For example, a repeatable-read transaction can keep seeing an order's old status after another transaction commits an update.
- **LSN**: log sequence number, a position in the recovery journal. It identifies replay and replication progress; only validated transaction boundaries are accepted as recovery targets.
- **OID**: object identifier used in PostgreSQL catalogs and type metadata.
- **prefix**: the beginning of object names assigned to one database inside a bucket. Separate prefixes keep their object names apart.
- **promotion**: transfer of durable write ownership to a fresh process incarnation by compare-and-swap.
- **PAX**: column-oriented row groups inside an SST, allowing selected-column reads. A query for order IDs need not fetch a large description column solely to read those IDs.
- **physical-demand proof**: the columns a query path may read from a physical row.
- **S3-compatible API**: the qualified HTTP data-plane subset implemented by MinIO and compatible object stores; it does not imply a vendor SDK, vendor control plane, or provider-specific implementation.
- **tuple identity value**: a PostgreSQL `tid` block/offset value carried at SQL and wire boundaries; it does not imply that an object-native row has a PostgreSQL heap address.
- **SST**: immutable sorted table of versioned rows, index, filter, and roster blocks. “Immutable” means a write creates replacement objects rather than editing published bytes in place.
- **snapshot**: the read position and transaction context that decide which row versions a read may see.
- **retention**: keeping bytes or objects while a reader, recovery path, or backup still needs them.
- **reclamation**: reusing memory or deleting old objects after they are no longer needed.
- **publication**: making prepared state visible through its authoritative reference. Uploading an object alone does not confirm a durable commit.
- **overlay**: newer in-memory row changes considered alongside checkpointed rows when answering a query.
- **compaction**: merging saved data into replacement objects so reads need fewer generations, while retaining versions still needed by readers.
- **pruning**: using safe summaries to skip data that cannot contain a match. Candidate matches still receive exact checks.
- **backpressure**: making incoming work wait or fail explicitly when fixed resources are busy, instead of growing memory use.
- **amplification**: extra physical work relative to useful work. If a 1 KiB update rewrites 100 KiB, its byte rewrite amplification is 100 times.
- **VOPR**: deterministic simulation that injects faults from a reproducible seed.
- **WAL**: checksummed local journal encoding used for recovery and logical replication; it is not PostgreSQL physical XLOG.
- **writer fence**: durable ownership record that prevents an older process incarnation from publishing either mutable root after promotion. A recovered old primary cannot overwrite the replacement's commits.
