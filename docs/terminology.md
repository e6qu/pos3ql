# Terminology and naming

This is the canonical glossary for pos3ql and the home of its naming rules.
Every other document links back here; when a term is unavoidable, it is defined
here rather than left implicit.

**Project documents** (all cross-linked): [README](../README.md) — architecture
and quick start · [PLAN](../PLAN.md) — roadmap and per-phase milestones ·
[BUGS](../BUGS.md) — known bugs and divergences · [AGENTS](../AGENTS.md) —
standing directives for anyone (human or AI) working here · this file — glossary
and naming rules.

## Naming rules

1. **Spell names out.** Identifiers — variables, functions, fields, types — use
   fully-qualified words: `interval`, not `iv`; `timestamp`, not `ts`; `buffer`,
   not `buf`; `expression`, not `expr`; `statement`, not `stmt`; `index`, not
   `idx`; `context`, not `ctx`; `format`, not `fmt`; `operator`, not `op`.
2. **Well-known acronyms are allowed** and stay upper- or lower-cased by
   convention: `AWS`, `ECS`, `S3`, `GCS`, `HTTP`, `TCP`, `TLS`, `SQL`, `JSON`,
   `UUID`, `URL`, `CPU`, `OS`, `ID`. These are recognized without expansion.
3. **Established short names for well-known concepts are allowed**, because a
   senior engineer reading this project already knows them — the module names
   (`exec`, `eval`, `mem`, `io`, `wal`, `guc`, `ast`, `vsr`, `pg`, `sim`)
   and a few universal computer-science idioms (`cmp`, `len`, `ptr`, `fd`). Each
   one is defined below so the choice is explicit, not assumed. Do not add new
   ones; anything not listed here gets spelled out.
4. **Project terminology is a last resort.** Prefer a plain description over a
   coined term. When a term is genuinely load-bearing and reused, it must be
   defined in the glossary below, and its first use in a module should be
   readable in context. Do not invent new short forms.
5. **Single-letter names** are acceptable only for a trivial, conventional loop
   or math index with no meaning of its own (`for i in 0..n`), or as the
   published variable names of a well-known algorithm (the civil-date
   conversion). A value that carries a meaning gets a name that states it.

These rules apply to new and edited code. Renaming existing identifiers is done
module by module, each change verified by `cargo build`, `cargo test`, and the
differential suite.

## Module map

The crate is one binary; these are its top-level modules (`src/<name>`). The
short names are kept (rule 3) and defined here.

- **`config`** — parse the configuration file and compute the static memory
  budget.
- **`mem`** (memory) — the slab, bump arenas, object pools, fixed-size
  collections, and the allocation guard. See *slab*, *arena*, *pool* below.
- **`io`** (input/output) — the I/O traits (clock, network, disk, object store)
  and their real (kqueue/epoll) and simulated drivers.
- **`vsr`** — Viewstamped Replication: messages, the replica state machine, the
  journal, view change, and recovery.
- **`wal`** — the write-ahead log / VSR journal format and replay.
- **`s3`** — the object-storage client (HTTP/1.1, AWS Signature Version 4,
  hand-rolled SHA-256 / HMAC, conditional writes, retries).
- **`pg`** (PostgreSQL) — the PostgreSQL wire protocol: framing, authentication,
  the simple and extended query flows, error responses.
- **`sql`** — the SQL front end and engine. Sub-modules: **`lexer`**,
  **`parser`** (into an **`ast`**, an abstract syntax tree), **`eval`**
  (expression evaluation), **`exec`** (statement execution), **`query`** (SELECT
  planning and joins), **`catalog`** (`pg_catalog` / `information_schema`),
  **`guc`** (session/server configuration parameters), **`numeric`**,
  **`datetime`**, **`range`**, **`timezone`**, **`to_char`**, **`regex`**.
- **`storage`** — the in-memory LSM write path: the *memtable* row heap, the
  row/table catalog, row encoding, and the visibility model. (There is no
  `lsm` module; the LSM is realized across `storage` + `checkpoint` + `wal`,
  and the leveled read/compaction half is roadmapped — see [PLAN.md](../PLAN.md).)
- **`checkpoint`** — snapshot live tables to SST objects and publish the
  compare-and-swap *manifest*; cold-start rehydration from the bucket.
- **`sim`** (simulator) — the deterministic whole-cluster simulator (VOPR).
- **`util`** (utilities) — small shared helpers (e.g. `StackStr`, a stack-backed
  string).

## Glossary

Terms that are unavoidable — mostly established database, consensus, and AWS
vocabulary — with the meaning they carry in this codebase.

### Storage and consensus

- **Viewstamped Replication (VSR)** — the consensus protocol (Liskov & Cowling,
  2012) pos3ql uses to replicate its operation log across replicas. A single
  node is a cluster of one.
- **Log-Structured Merge tree (LSM)** — the storage model: writes land in an
  in-memory table and are later flushed to immutable sorted files, with
  background compaction. *Current state:* pos3ql implements the in-memory half
  (the memtable) plus checkpoint SSTs; memtable flush, leveled SSTs, and a
  queryable multi-level read path with compaction are roadmapped (PLAN.md).
- **memtable** — the in-memory, sorted write buffer at the top of the LSM.
  Today it holds the whole live working set (`memtable_bytes`); flushing a
  frozen memtable to an SST is roadmapped.
- **Sorted String Table (SST)** — an immutable, sorted, block-structured object
  of rows on object storage. Today an SST is a per-table *checkpoint snapshot*;
  block-granular SSTs read a block at a time are roadmapped.
- **block** — the fixed-size, checksummed, content-addressed unit the roadmapped
  storage engine reads and caches: SST data/index/filter blocks, the manifest
  log, and cache frames are all blocks (after TigerBeetle's *grid*). Roadmap.
- **block grid** — the fixed array of blocks unifying on-object and cached
  storage; the seam (`BlockStore`) with a local backend and an object-storage
  backend. Roadmap.
- **block cache / disk cache** — the RAM and local-disk read-through tiers in
  front of object storage (`block_cache_bytes` / `disk_cache_bytes`), the
  "ClickHouse/Loki-style" cache of the founding vision. Reserved in config
  today; wired in the roadmap.
- **manifest log** — an append-only log of SST-added/removed records rooted by a
  CAS'd *superblock*, replacing the monolithic manifest rewrite (after
  TigerBeetle's `manifest_log` / Loki's index shipping). Roadmap.
- **superblock** — the single compare-and-swap root object naming the manifest
  log's tail; the storage engine's linearization point. Roadmap.
- **content-addressed** — an object keyed by the hash of its bytes, hence
  immutable and safe to cache indefinitely and to reference across an eventually
  consistent LIST.
- **leveled / tiered compaction** — the two LSM compaction shapes (low
  read-amplification vs low write-amplification) weighed against object-storage
  economics in the roadmap. Roadmap.
- **Write-Ahead Log (WAL)** — the durable operation log; here it doubles as the
  VSR journal. Replayed on recovery.
- **manifest** — the single object-storage object naming the current set of SSTs
  and the catalog root, updated by a compare-and-swap conditional write.
- **compare-and-swap (CAS)** — a conditional object-storage write
  (`If-Match` / `If-None-Match`) used so a lagging or split-brained node cannot
  clobber the manifest.
- **Log Sequence Number (LSN)** — the monotonically increasing position of a
  committed operation; snapshots are LSNs.
- **Multi-Version Concurrency Control (MVCC)** — snapshot isolation via
  per-version visibility, keyed by transaction id and LSN.
- **tombstone** — a marker recording that a key was deleted, carried through
  compaction until it can be dropped.
- **transaction id (`transaction_id`)** — the identifier of a transaction's
  snapshot, used for row visibility. (Field name: previously `txid`.)

### Object-storage read path & execution (roadmap)

The performance vocabulary of the object-storage LSM and its adaptive executor
(see [PLAN.md](../PLAN.md), Stages A–I). All roadmap terms.

- **SSTable** — long form of *SST* above; the immutable sorted block-structured
  object of rows on object storage.
- **sparse index** — one entry per SST data block (its first key → block offset),
  small enough to keep resident so a lookup finds the single block to fetch
  without scanning the SST.
- **bloom filter** — a compact probabilistic set summary stored per SST; a
  "not present" answer is certain, so a point lookup skips an SST that cannot
  hold the key (no false negatives; rare false positives).
- **zone map** (min-max) — the stored minimum and maximum of a column per block
  or SST, letting the planner *prune* blocks a predicate cannot match.
- **read amplification / write amplification** — the extra blocks read per
  logical read, and extra bytes written per logical write; the two costs an LSM
  compaction shape trades off (leveled minimizes read-amp, tiered write-amp).
- **prefetch / read-ahead** — issuing block GETs ahead of the scan cursor,
  concurrently, so object-storage latency is overlapped into throughput.
- **hedged request** — issuing a duplicate GET once the first passes a latency
  deadline and taking whichever returns first, to cut object-storage tail latency.
- **late materialization** — carrying only the key and the columns a stage needs,
  assembling full rows only for those that survive filters/joins/LIMIT, so fewer
  blocks are fetched.
- **vectorized execution** (block-at-a-time) — operators processing a whole
  block's worth of rows per step in a push-based batched pipeline, rather than one
  row per call, for throughput.
- **PAX** (row-group) — a within-block layout that keeps rows grouped by key but
  clusters columns inside the block (Parquet-row-group style), enabling column
  projection and better compression without leaving the row model.
- **free set** — the fixed structure tracking which grid blocks are free vs.
  allocated (after TigerBeetle's grid free set).
- **cost model** — the planner's estimate of a plan's cost; the *storage-aware*
  version prices object-storage request latency, request count, bandwidth, and
  cache residency so the planner adapts plans to the bucket.

### Memory discipline

- **slab** — the single, up-front reservation of the process's entire memory
  budget, partitioned into arenas and pools at startup.
- **arena** — a bump-allocated region handed out from the slab; the per-statement
  SQL arena is reset between statements. No heap allocation happens after
  startup.
- **pool** — a fixed-count set of reusable fixed-size objects drawn from the
  slab.

### Object storage and crypto

- **AWS Signature Version 4 (SigV4)** — the request-signing scheme for
  authenticating to S3-compatible object storage.
- **object storage** — an S3-compatible service (AWS S3, GCS, MinIO); the
  durable home of SSTs, WAL segments, and the manifest.

### SQL and wire

- **object identifier (OID)** — PostgreSQL's numeric type/relation identifier,
  sent on the wire and stored in the catalog. `OID` is treated as a well-known
  term; the spelled-out form is used in prose.
- **Grand Unified Configuration (GUC)** — PostgreSQL's name for a session or
  server configuration parameter (`statement_timeout`, `TimeZone`, …).
- **datum** — a single typed SQL value (`Datum`); the unit the executor moves
  around. Kept as a proper noun (PostgreSQL's own term).
- **catalog** — the set of `pg_catalog` / `information_schema` relations pos3ql
  synthesizes so real drivers can introspect.
- **transaction id (`transaction_id`)** — identifies a transaction's snapshot,
  used for row visibility under MVCC.
- **display scale (`dscale`)** and **`ndigits`** — arbitrary-precision numeric
  internals matching PostgreSQL's `numeric.c`: `dscale` is the number of
  fractional digits to display, `ndigits` the count of base-10000 digit words.
  Kept because they name the exact PostgreSQL fields being reproduced.
- **type modifier (`typmod`)** — PostgreSQL's per-column type parameter (e.g. the
  `n` in `varchar(n)`); kept as PostgreSQL's own field name.

### Rust and systems idioms

Universal short names a systems engineer reads without expansion (rule 3); kept.

- **`cmp`** — a three-way comparison (`Ordering`), after Rust's `Ord::cmp`.
- **`len`** — a length/count, after Rust's `.len()`.
- **`ptr`** — a raw pointer.
- **`fd`** — a file descriptor (the integer a `kqueue`/`epoll` loop waits on).
- **`StackStr`** — the utility fixed-capacity, stack-backed string (no heap
  allocation), used pervasively for bounded text.

### Testing

- **VOPR** — the Viewstamped-Operation deterministic simulator: whole-cluster
  simulation with fault injection, reproducible from a seed.
- **PCG** — the permuted-congruential pseudo-random number generator used so
  every simulated run is reproducible from its seed.
- **differential testing** — running the same SQL against real PostgreSQL and
  against pos3ql and diffing the results; PostgreSQL is the oracle. See
  [BUGS.md](../BUGS.md) for the divergences it has surfaced.
