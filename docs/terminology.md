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
  hand-rolled SHA-256 / HMAC, conditional writes, retries, the isolated
  rustls TLS door), plus `s3::sim`, the deterministic *virtual bucket*
  behind the same client seam (see Testing below).
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
- **`sim`** (simulator) — the deterministic simulators (VOPR): the VSR
  whole-cluster simulator, and the storage VOPR (`sim::storage`) driving the
  engine's storage stack against the virtual bucket.
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
  background compaction. *Current state:* memtable + block SSTs with
  spill-under-pressure, delta flushes with tombstones, and paced pair
  merges; the remaining distance (a block-resident row map, beat-paced
  merges) is the maturity roadmap in [PLAN.md](../PLAN.md).
- **memtable** — the in-memory, sorted write buffer at the top of the LSM
  (`memtable_bytes`). Under memory pressure committed row *bytes* spill to
  the bucket and page back through the cache tiers; the per-row map stays
  in RAM (making it block-resident is the maturity roadmap's gap 2).
- **Sorted String Table (SST)** — an immutable, sorted, block-structured
  object of rows on object storage: sorted data blocks + a sparse index
  block + a bloom filter block, read block-at-a-time through the cache
  tiers. Checkpoints write per-table block SSTs; delta flushes append to a
  table's SST list and paced merges bound it.
- **block** — the fixed-size (256 KiB), checksummed, content-addressed unit
  the storage engine reads and caches: SST data/index/filter/roster blocks
  (after TigerBeetle's *grid*), identified by the SHA-256 of the payload.
- **block grid** — the array of blocks unifying on-object and cached
  storage; the seam (`BlockStore`) with object-storage, RAM, disk-cache and
  in-memory-test backends stacked by `store::build_tiers`.
- **block cache / disk cache** — the RAM and local-disk read-through tiers in
  front of object storage (`block_cache_bytes` / `disk_cache_bytes`), the
  "ClickHouse/Loki-style" cache of the founding vision: CLOCK-evicted RAM
  frames over a preallocated slot file, both pure cache (a torn or stale
  slot is a miss, never data loss).
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
- **sweep / beat** — a checkpoint *sweep* is the whole unit of work from
  trigger to manifest publish; a *beat* is one bounded step of background
  work run between query messages (and by the idle event loop): one
  table's slice, one stretch of a paced merge, or the publish itself. The
  publish beat runs only when no table has changed since its slice, which
  per-table generations guarantee. Merge beats and sweep beats alternate
  when both want the engine. The word *beat* is TigerBeetle's, for the
  same amortize-the-work idea.
- **paced merge (merge job)** — compaction as a background job crossing
  beats: a table's spill list at the merge trigger gets its cheapest
  adjacent pair merged — the schedule built a few block *reads* per beat,
  the output streamed a few block *writes* per beat — surviving publishes
  (a delta only appends at the tail, so the pair's positions hold) and
  cancelled without loss if a collapse supersedes the pair. The half-built
  SST's blocks join the garbage sweep's keep-set until the job's result
  publishes.
- **Write-Ahead Log (WAL)** — the durable operation log; here it doubles as the
  VSR journal. Every committed row image and definition change is appended
  and fsynced before the commit acknowledges (the local durability point),
  and replay after a crash rebuilds state from the last checkpoint forward.
- **WAL segment** — a committed WAL batch uploaded to the bucket as one
  object, keyed by its first LSN. A segment only *grows* under its key: a
  failed upload keeps the batch marker, so the retry carries the old bytes
  plus newly committed ones. Cold start replays segments newer than the
  manifest; a checkpoint prunes segments it made redundant.
- **commit-durable-on-bucket** — the default posture with `s3 = on`
  (`wal_upload_sync`): a commit acknowledges only after its WAL segment is
  in the bucket, so wiping the local disk at any instant loses nothing
  acknowledged — the disk is formally a cache.
- **RPO (recovery point objective)** — how much acknowledged work a
  disaster may lose. RPO=0 against total node loss is what
  commit-durable-on-bucket buys; the asynchronous drain trades a
  drain-window of RPO for local-fsync commit latency.
- **group commit** — amortizing the per-commit durability round-trip by
  covering many concurrent commits with one fsync/PUT before their acks.
  The WAL's local fsync already batches per transaction; the
  cross-connection segment-PUT form is deferred to the reactor's async
  work (it requires holding acks, and every ack already follows its PUT).
- **manifest** — the single object-storage object naming the current set of SSTs
  and the catalog root, updated by a compare-and-swap conditional write.
- **compare-and-swap (CAS)** — a conditional object-storage write
  (`If-Match` / `If-None-Match`) used so a lagging or split-brained node cannot
  clobber the manifest.
- **Log Sequence Number (LSN)** — the monotonically increasing position of a
  write in the WAL, assigned per journaled record and never reused or reset
  (a truncated journal keeps counting from where it was). It is the
  engine's clock for durability questions: the manifest names the LSN its
  snapshot covers, WAL replay applies only records newer than the manifest
  it loaded (the monotonic-LSN rule that makes replay idempotent), WAL
  segments are keyed by their batch's first LSN, and the roadmap's
  LSN-keyed MVCC will stamp row versions with their commit LSN so a
  snapshot is just an LSN.
- **Multi-Version Concurrency Control (MVCC)** — letting readers and
  writers coexist by keeping more than one version of a row, each reader
  seeing the versions its snapshot admits rather than blocking writers.
  *Today's model* is the minimal two-version form: each row is at most one
  committed image plus one uncommitted pending image, visibility decided by
  transaction id (READ COMMITTED — every statement sees what was committed
  when it started), and a second concurrent writer fails fast with `40001`
  rather than blocking. *The roadmap's model* (Stage F) is genuine
  LSN-keyed versioning: versions appended (never repointed in place)
  stamped with their commit LSN, a read at snapshot `S` taking the newest
  version with `commit_lsn <= S`, and compaction retaining any version
  still visible to the *oldest live snapshot* watermark. It lands with the
  suspendable row source (Stage I), the first place a reader can outlive a
  statement and so first needs a version history.
- **snapshot** — the point in time a read is served *as of*. Under READ
  COMMITTED it is per-statement (identified by transaction id today); under
  the roadmap's LSN-keyed MVCC it is literally an LSN.
- **oldest live snapshot (watermark)** — the earliest snapshot any live
  reader still holds; compaction may drop a superseded row version only
  once it falls below this watermark. (With no suspendable readers, the
  watermark today is always "now", which is why compaction needs no
  version retention yet.)
- **write-write conflict (`40001`)** — two transactions writing the same
  row: the second fails immediately with SQLSTATE `40001` (serialization
  failure) instead of blocking on a lock the way PostgreSQL's READ
  COMMITTED would; applications retry.
- **tombstone** — a marker recording that a key was deleted, written into
  delta SSTs so an older member's version stays dead at cold start, and
  carried through merges until the pair sits at the head of the list —
  where nothing older remains to suppress, and it is dropped.
- **spill / spill list** — under memory pressure a committed row's *bytes*
  leave the heap: the row's map entry flips to `Spilled`, naming which
  member of its table's **spill list** (the ordered SSTs in the manifest's
  `dsst` lines) holds them; reads fetch them back through the cache tiers.
  Later members shadow earlier ones; tombstones delete.
- **delta flush** — a dirty table with spilled SSTs checkpoints only its
  heap-resident committed rows plus the tombstones recorded since the last
  checkpoint, appended as one new list member — instead of rewriting
  everything it already made durable.
- **roster** — one block per SST listing every block identity the SST
  comprises, so the garbage sweeper enumerates an SST with a single read.
- **generation** (`Table::mark_dirty`) — a per-table counter bumped on
  every committed change; the sliced checkpoint compares it against the
  generation its slice captured, which is how a publish knows no table
  changed after its slice.
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

- **VOPR** — a deterministic fault-injecting simulator whose every run
  reproduces exactly from a PRNG seed. The name is TigerBeetle's ("Viewstamped
  Operation Replicator", their simulator of the VSR cluster — itself a nod to
  the WOPR of *WarGames*); here it names the discipline: drive the real code
  through simulated infrastructure, inject faults from a seeded PRNG, and
  check invariants after every recovery. pos3ql has two: the *VSR VOPR*
  (`sim`, the consensus cluster under message loss/reorder/partition) and the
  *storage VOPR* below.
- **storage VOPR** (`sim::storage`) — the storage-stack simulator: the real
  `Engine` (WAL, spill, checkpoint, block SSTs, cache tiers, manifest CAS,
  garbage sweep) runs against the *virtual bucket* under seeded fault
  schedules — transient failures, ambiguous PUTs, flipped bits, outages
  ending in crashes, corrupted disk-cache slots, warm restarts and
  wiped-disk cold starts — while a model database tracks every acknowledged
  write and recovery is checked against it exactly.
- **virtual bucket** (`s3::sim`, `s3 = sim`) — the deterministic in-process
  object store standing in for S3 behind the object-client seam. It also
  enforces the bucket-side key discipline itself (see *blind overwrite*).
  Refused by the real server binary; it exists for simulation tests.
- **fault plan** — the virtual bucket's per-operation fault dice (parts per
  thousand) plus the outage schedule (`fail_from_op`), all drawn from one
  PCG stream so a failing run replays from its seed.
- **ambiguous PUT** — a write that landed but whose response was lost: the
  caller sees an error and cannot know the object exists. The classic
  object-storage failure; retries and commit acknowledgements must both
  treat the outcome as unknown, never as "did not happen".
- **blind overwrite** — an unconditional PUT that changes an existing
  object's bytes. The engine's key discipline forbids it (blocks are
  content-addressed, the manifest moves only by CAS, a WAL segment only
  grows by appended commits under its first-LSN key), so the virtual bucket
  records one as a failed invariant.
- **crash torture** (`tests/external/torture_diff.py`) — the process-level
  sibling of the storage VOPR: seeded random DML against pos3ql *and* a real
  PostgreSQL, with random `kill -9` restarts and wiped-disk cold starts,
  the reference database serving as the model.
- **PCG** — the permuted-congruential pseudo-random number generator used so
  every simulated run is reproducible from its seed.
- **differential testing** — running the same SQL against real PostgreSQL and
  against pos3ql and diffing the results; PostgreSQL is the oracle. See
  [BUGS.md](../BUGS.md) for the divergences it has surfaced.
