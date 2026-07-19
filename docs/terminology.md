# Terminology and naming

This is the canonical glossary for pos3ql and the home of its naming rules.
Every other document links back here; when a term is unavoidable, it is defined
here rather than left implicit.

Related documents: [README](../README.md) · [PLAN](../PLAN.md) ·
[BUGS](../BUGS.md) · [AGENTS](../AGENTS.md).

## Naming rules

1. **Spell names out.** Identifiers — variables, functions, fields, types,
   modules — use fully-qualified words: `interval`, not `iv`; `timestamp`, not
   `ts`; `buffer`, not `buf`; `expression`, not `expr`; `statement`, not `stmt`;
   `index`, not `idx`; `context`, not `ctx`; `format`, not `fmt`.
2. **Well-known acronyms are allowed** and stay upper- or lower-cased by
   convention: `AWS`, `ECS`, `S3`, `GCS`, `HTTP`, `TCP`, `TLS`, `SQL`, `JSON`,
   `UUID`, `URL`, `CPU`, `OS`, `ID`. These are recognized without expansion.
3. **Project terminology is a last resort.** Prefer a plain description over a
   coined term. When a term is genuinely load-bearing and reused, it must be
   defined in the glossary below, and its first use in a module should be
   readable in context. Do not invent new short forms.
4. **Single-letter names** are acceptable only for a trivial, conventional loop
   or math index with no meaning of its own (`for i in 0..n`). A value that has
   a meaning gets a name that states it.

These rules apply to new and edited code. Renaming existing identifiers is done
module by module, each change verified by `cargo build`, `cargo test`, and the
differential suite.

## Glossary

Terms that are unavoidable — mostly established database, consensus, and AWS
vocabulary — with the meaning they carry in this codebase.

### Storage and consensus

- **Viewstamped Replication (VSR)** — the consensus protocol (Liskov & Cowling,
  2012) pos3ql uses to replicate its operation log across replicas. A single
  node is a cluster of one.
- **Log-Structured Merge tree (LSM)** — the storage model: writes land in an
  in-memory table and are later flushed to immutable sorted files, with
  background compaction.
- **memtable** — the in-memory, sorted write buffer at the top of the LSM. On
  flush it becomes a sorted string table.
- **Sorted String Table (SST)** — an immutable, sorted, block-structured file of
  rows on object storage; the on-disk (on-S3) form of flushed data.
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
- **datum** — a single typed SQL value.
- **catalog** — the set of `pg_catalog` / `information_schema` relations pos3ql
  synthesizes so real drivers can introspect.

### Testing

- **VOPR** — the Viewstamped-Operation deterministic simulator: whole-cluster
  simulation with fault injection, reproducible from a seed.
- **PCG** — the permuted-congruential pseudo-random number generator used so
  every simulated run is reproducible from its seed.
- **differential testing** — running the same SQL against real PostgreSQL and
  against pos3ql and diffing the results; PostgreSQL is the oracle.
