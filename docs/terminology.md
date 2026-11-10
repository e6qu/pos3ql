# Terminology and naming

## Naming

- Spell identifiers out: `interval`, `buffer`, `expression`, `statement`, `index`.
- Allowed acronyms: SQL, WAL, SST, LSN, OID, HTTP, TLS, UUID, JSON.
- Allowed short module names: `ast`, `eval`, `exec`, `guc`, `io`, `mem`, `pg`, `sim`, `sql`, `wal`.
- Coin a term only for a repeated, specific meaning, and define it here.
- Single-letter names only for conventional loop indices and published algorithm notation.

## Modules

| Module | Contents |
|---|---|
| `config` | configuration and memory budget |
| `mem` | fixed-capacity memory, pools, arenas, allocation guard |
| `io` | operating-system and simulated I/O |
| `pg` | PostgreSQL wire protocol |
| `sql` | parser, executor and catalogs |
| `storage` | resident catalog, row state, visibility |
| `wal` | durable journal encoding and replay |
| `checkpoint` | SST publication and cold recovery |
| `store` | block formats and cache tiers |
| `object_store` | S3-compatible HTTP client, request signing, TLS |
| `crypto` | SHA-256 and HMAC |
| `sim` | deterministic fault simulation |

The [architecture](architecture.md) describes how they interact.

## Glossary

User guidance should use ordinary words and explain a term at first use; these
definitions keep technical text consistent.

- **amplification**: extra physical work per unit of useful work. A 1 KiB update that rewrites 100 KiB has 100× byte rewrite amplification.
- **backpressure**: making new work wait or fail explicitly when fixed resources are busy, instead of growing memory.
- **backup**: a named, checkpoint-consistent manifest and commit head whose blocks and commit history are retained until the backup is deleted.
- **backup export**: a restartable copy of one backup's objects into an empty, separately configured object-store prefix.
- **block**: a checksummed, content-addressed storage unit of at most 256 KiB including its header.
- **block store**: the internal provider-neutral interface over object storage and cache tiers; not a network service.
- **checkpoint**: a saved layout of table data that shortens recovery; later commits are replayed from the commit history.
- **cold start**: recovery with RAM and local disk caches empty.
- **commit batch**: immutable journal bytes plus a descriptor, recoverable only once the commit head names it.
- **compaction**: merging saved data into fewer replacement objects while keeping versions readers still need.
- **compare-and-swap (CAS)**: updating a saved reference only if it still has the generation previously read, so an old writer cannot replace newer state.
- **declared type**: the schema-qualified type shown in catalog, parameter and replication metadata, as distinct from the executor's value type.
- **durable mode**: `object_store = on`; a commit is acknowledged only after its commit batch is published.
- **ETag**: the object store's opaque, quoted generation token used for compare-and-swap.
- **execution control pool**: fixed startup memory shared by running procedural calls for their positions, local variables, source images and error state.
- **LSN**: log sequence number, a position in the journal used for replay and replication progress.
- **manifest**: the durable inventory of table definitions and storage objects for a checkpoint, updated by compare-and-swap.
- **MVCC**: multiversion concurrency control: keeping row versions so each transaction sees the versions its isolation level allows.
- **OID**: object identifier used in PostgreSQL catalogs.
- **overlay**: newer in-memory row changes combined with checkpointed rows when answering a query.
- **PAX**: column-grouped row blocks inside an SST, so a query reads only the columns it needs.
- **prefix**: the start of object names reserved for one pos3ql instance in a bucket.
- **promotion**: moving durable write ownership to a fresh process by compare-and-swap.
- **pruning**: skipping data that summaries prove cannot match; candidates are still checked exactly.
- **publication**: making prepared state visible through its authoritative reference. Uploading an object alone does not commit it.
- **reclamation**: reusing memory or deleting objects once nothing needs them.
- **retention**: keeping bytes or objects while a reader, recovery or backup still needs them.
- **S3-compatible API**: the HTTP data-plane subset qualified against MinIO and compatible stores; no vendor SDK or control plane.
- **snapshot**: the read position and transaction context that decide which row versions a read sees.
- **SST**: sorted string table: an immutable object of versioned rows with index, filter and roster blocks. Writes create replacement objects instead of editing published ones.
- **value slot**: one typed SQL value entry; large values refer to bytes stored elsewhere, which the slot does not own.
- **VOPR**: deterministic simulation that injects faults from a reproducible seed.
- **WAL**: the checksummed journal encoding used for recovery and logical replication; not PostgreSQL's physical WAL format.
- **writer fence**: a durable ownership record that stops an older process from publishing after promotion.
