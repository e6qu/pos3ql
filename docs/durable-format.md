# Durable format compatibility

This guide lists the object formats the current binary reads and writes, and
the rules it uses to reject anything else. [Architecture](architecture.md)
explains the storage layout and physical limits; this page is the byte-level
inventory.

Object storage is the durable tier, and immutable objects can outlive the
binary that wrote them. Every root, journal record, block and row SST
(sorted string table, an immutable sorted run of rows) therefore carries a
format identity that is checked before its bytes are interpreted. An unknown
identity stops startup with a named error; it never selects a guessed decoder.

Compatibility with earlier pos3ql versions is not provided. There is no
automatic upgrade, downgrade or offline migration.

**User example:** after an incompatible format change, a database saved by an
older binary fails startup. Use a fresh object-store prefix, or an explicitly
verified logical export and restore between the two binaries. Never point a new
database at an old prefix and assume its objects are disposable. Application
schema changes follow a different procedure; see
[operations](operations.md#scheduled-work-and-schema-migrations).

## Current compatibility matrix

| Object | Identity | Read | Write | Notes |
|---|---|---:|---:|---|
| Checkpoint manifest | `pos3ql-manifest-v41` | yes | yes | Only accepted manifest; every other header is rejected before its references are used. |
| Journal record | Version `32` at header byte 17; bytes 18–23 are zero | yes | yes | Other versions are rejected even when the checksum is valid. |
| Immutable block | Header bytes 5–7: `01 00 00` | yes | yes | Identity is SHA-256 of `pos3ql:block:v1\0`, the one-byte block type, then payload; the LSN is provenance, excluded from identity. |
| Empty checkpoint root | `pos3ql-empty-manifest-v1` | yes | yes | Fences an as-yet unpublished manifest root. |
| Commit head | `pos3ql-commit-head-v1` | yes | yes | Names the immutable commit chain and the publishing process's writer token. |
| Empty commit root | `pos3ql-empty-commit-head-v1` | yes | yes | Fences an as-yet unpublished commit root. |
| Writer fence | `pos3ql-writer-fence-v1` | yes | yes | Serializes promotion and mutable-root publication. |
| Backup completion | `pos3ql-backup-v1` | yes | no | Named-point and LSN restore only; it has no backup creation timestamp. |
| Backup completion | `pos3ql-backup-v2` | yes | yes | Checksums both roots and anchors timestamp recovery after backup creation. |
| Point-in-time restore marker | `pos3ql-point-in-time-restore-v1` | yes | yes | Binds the backup, manifest checksum, resolved LSN and commit head during root replacement. |
| Backup export marker | `pos3ql-export-v1` | yes | yes | Pending and completion records make an independent-prefix copy restartable. |
| Published row SST | `v2` | yes | no | Direct content-addressed data blocks; replaced when sliced or merged. |
| Published row SST | `v3` | yes | no | Packed PAX (column-grouped) descriptors with verified column extents. |
| Published row SST | `v4` | yes | yes | Current: PAX full slices plus compressed row-packed deltas. |

A test in `src/checkpoint.rs` compares the manifest and row SST rows above with
the executable reader and writer declarations, so changing an identity requires
changing the code, this table and the tests together.

## Rejection rules

- Unknown identities, malformed references and formats outside the matrix fail
  before their bytes are interpreted. No conversion fallback exists.
- A checksum-valid journal record with an unsupported version stops local replay
  without truncating the journal; uploaded records validate the same version.
- A recognized block type inside the wrong row SST format is corruption, not an
  alternate decoding path. Column containers and row indexes with identical bytes
  are still different blocks, because the block type is part of the object name.
- Within the current version, every reachable object, prepared transaction and
  backup root must remain readable; a checkpoint or retry may not discard objects
  that current readers or backups still need.
- Startup validates the whole saved catalog and its immutable references before
  promoting the writer. Invalid catalog images fail with `22000`; malformed
  records leave the manifest, commit head and writer fence unchanged.
- Missing, duplicate, zero, out-of-range or truncated identities are rejected;
  recovery never guesses or regenerates a missing identity.
- Recovery fails with `22000` when a valid partitioned index lacks exactly one
  attached index on a partition that can hold one, or an index is attached to
  a parent index outside its table's partition parent. Earlier releases could
  save partitions without their copy of a partitioned unique index, leaving
  duplicates unchecked there; such a state is refused, not repaired.

### Change checklist

A durable-format change is complete only when it includes:

- a new stable identity and one parse boundary for it;
- reads of every declared format and object-cold recovery (recovery with empty
  local caches);
- checkpoint retry and compare-and-swap publication coverage;
- exact fixed-memory accounting for readers, writers and any conversion scratch;
- garbage retention for both the published root and in-progress output;
- the accepted and rejected identities in the matrix above; and
- a manifest-capacity assessment for any added reference bytes.

Do not keep a decoder or add a conversion solely for an older pos3ql version.

## Format details for implementers

### Journal records

Framing is 24 bytes: CRC32C, payload length and LSN in bytes 0–15; byte 16 is
the operation kind; byte 17 is the version (`32`); bytes 18–23 are zero.
Field-width changes use new kind bytes. Kinds with special rules:

| Kind | Meaning | Rule |
|---|---|---|
| 123 | Parameter permissions | Name, expected old row identifier, then removal or a complete replacement row. Each grant: two 32-bit role identifiers, one byte each for privileges and grant options. An empty row differs from removal (default permissions). |
| 139/140 | Publication create/alter | Current; wide column sets. Alter kind 129 remains readable; create kind 128 is rejected. |
| 141 | Trigger create | Current; requires the ten-byte position prefix. Kind 55 remains readable. |
| 142 | Routine create | Current; 16-bit result counts. Kind 45 remains readable. |
| 143 | Composite create | Current. Kind 58 remains readable. |
| 144/145 | View create / view columns | Current; 16-bit column count. Each column's name is followed by its type (four-byte type OID, two-byte length, four-byte modifier, four-byte collation code), then its default and the default's four-byte `pg_attrdef` stamp. Kinds 5/126, whose columns have no types, are rejected. A view create ends with its options, which view-option changes (kind 127) carry alone: security, barrier and check-option codes, a query-origin byte (0 `CREATE VIEW`, 1 `CREATE OR REPLACE RULE "_RETURN"`), then the `reloptions` list (order code, then each listed option's written value: length and eight padded bytes). An order that disagrees with the codes is rejected. |
| 148 | Composite rename or schema move | Prior name, prior schema, new schema, new name. Preserves current fields. |
| 149 | Enum create | Current. Kinds 23 and 147 are rejected. |
| 150 | Extended statistics ANALYZE | Current. Kind 77 enum-dependent hash summaries are retired; counts stay valid. |
| 151 | Domain with bound checks, defaults or named array base | Kind 21 remains readable. |
| 152 | Bound view | Kind 144's layout for a query with bound enum literals. |
| 153 | Row upsert | Allows tagged enum member cells and legacy cells. Kind 3 remains readable. |
| 154 | Table with bound default source | Kind 1 remains readable. |
| 155 | Prepared catalog row | See [prepared transactions](#prepared-transactions). |
| 156 | Index-presence hint | Ten-byte table position, then a strict 0/1 flag. |
| 157 | Catalog reservation | Eight-byte nonzero creation-counter reservation, kept even when a transaction creates and removes all its images. |
| 158 | Inherited trigger change | Flags byte, then expected/replacement images: four-byte clone stamp, ten-byte trigger position, ten-byte table position, one-byte enabled mode. |
| 159 | Built-in routine attributes | Four-byte database identifier, four-byte built-in routine OID, then strict, volatility, parallel, security-definer and leakproof bytes, cost and rows as IEEE-754 bits, a settings count (at most 16) and each `SET` setting's name and value, each after a one-byte length. Owner and privileges use the object owner and ACL kinds with the routine OID. |
| 160 | Trigger alteration | Ten-byte position, prior name, target kind, table schema and name, new name, enabled mode, then the complete `WHEN` condition (two-byte length plus one, zero when absent). Kind 57, without the condition, is rejected. |
| 161 | Index alteration | Schema, index name and mutable settings, then the complete key expressions (four-byte presence mask over the 32 key positions; each present expression as a two-byte length and text) and predicate (flag byte, then two-byte length and text). Kind 71, without the expressions, is rejected. |
| 171 | Foreign-data wrapper | Slot, creation stamp, owner, presence byte, then name, the handler's and validator's four-byte `pg_proc` OIDs (0 for none) and the options. Kind 115, with one postgres_fdw flag byte each, is rejected. |

Other payload rules:

- **Row deletion (kind 4)** has a flags byte after the schema name: bit 0
  means the removed image follows, bit 1 means the deletion moved the row to
  another partition. Other bits are rejected. Recovery and restored prepared
  transactions keep the move flag, so a statement that waited for the row
  fails with PostgreSQL's `40001` instead of skipping it.
- **Table definition payload** is version 20: version byte, ten-byte catalog
  position, name, then the definition. Other versions are rejected. Each
  column carries a generation-kind byte after its NOT NULL inheritance byte
  (0, or the `attgenerated` code `s` or `v`), which must agree with the
  generated flag. After its validation code, each foreign key stores four
  one-byte `tgenabled` codes (`O`, `R`, `A`, `D`) for its delete-action,
  update-action, insert-check and update-check RI triggers, then a one-byte
  count and the two-byte column positions its `ON DELETE SET NULL (...)` or
  `SET DEFAULT (...)` list changes (PostgreSQL's `confdelsetcols`; zero means
  every key column). The manifest's `fkey` line carries the same four codes,
  count and positions after the validation code. The definition ends with its
  [expression dependencies](#expression-dependencies).
- **Domain payload** uses marker 251; earlier layouts are rejected. A mandatory
  checked-default input section follows the constraint identities (an empty
  section is explicit).
- **Table statistics** use V5 marker 251, a declared record count and strictly
  increasing column numbers 1–1,600. V3/V4 images are rejected.
- **Commit boundaries** append PostgreSQL-epoch commit microseconds to the
  transaction identity fields. The empty, identity-only and
  identity-plus-assignment forms are still read; LSN recovery can use them, but
  timestamp recovery rejects a boundary whose time is unknown.
- **Schema records** begin with the eight-byte creation stamp, validated before
  renaming or dropping.
- **Subscription creation** begins with the nonzero eight-byte little-endian
  creation identity, so replication progress always binds to the same
  subscription regardless of replay allocation order.
- **Publication creation** places the eight-byte stamp before the name.
- **Sequence restart** records an explicit presence byte and an optional
  eight-byte value; a prepared restart stays private until commit.
- **Role definitions** start with a four-byte identifier and two-byte catalog
  slot. Prepared-transaction owners use that four-byte identifier.

Recovery may write an immutable prefix of a multi-transaction commit batch. Its
identity is the CRC32C of the exact prefix and its descriptor keeps the original
predecessor, so the published head always ends at a whole validated transaction.

### Checkpoint manifest

The header versions the whole record grammar. One allocation-free metadata
parser serves startup, backup retention and writer promotion. It requires exactly
one each of `lsn`, `next_rowid`, `latest_transaction_id`, `next_lo_oid`,
`catalog_sequence`, `role_sequence`, a nonzero 32-digit hexadecimal `writer`,
`types_end` and the final `end`. Missing, repeated, malformed or trailing
metadata fails before catalog restoration or root publication; promotion never
inserts a missing writer.

| Record | Contents and rules |
|---|---|
| `table2` | Slot, creation stamp, hexadecimal checked-input image, name. Internal large-object page table keeps its reserved slot and stamp zero. |
| `sq7`, `rtn2` | Sequence / routine slot and creation stamp. A routine's language code is 0 (SQL), 1 (PL/pgSQL), 2 (internal aggregate) or 16 plus a native routine code for a `LANGUAGE C` routine bound to built-in postgres_fdw code (0 handler, 1 validator, 2 and 3 `get_connections` 1.1 and 1.2, 4 `disconnect`, 5 `disconnect_all`). Such a handler's `fdw_handler` result persists as type code 250. |
| `fdw2` | Foreign-data wrapper slot, creation stamp, owner, handler and validator `pg_proc` OIDs (0 for none), name and options. The earlier `fdw` record, with 0/1 flags, is rejected. |
| `xpk`, `xsc` | Available extension packages and their version scripts, numbered after the packages built into the binary (`$system`'s postgres_fdw), which are never persisted. |
| `mv6` | Materialized-view catalog position and backing-table position. |
| `vw16` | View position and stamp, return-rule position and stamp, encoded query, schema, search path, name, then the security, barrier, check-option and query-origin codes (query origin: 0 `CREATE VIEW`, 1 `CREATE OR REPLACE RULE "_RETURN"`; it decides whether `pg_get_viewdef` qualifies columns), the `reloptions` list as hexadecimal (order code, then each option's written value: length and eight padded bytes), then each column by ordinal: name, checked default, `pg_attrdef` stamp (zero without a default), type OID, type length, type modifier and collation code. |
| `hidx <table-slot> <0-or-1>` | Required once per table, including tables without indexes. The large-object page table requires `0`. |
| `col5` | Type, flags, then the generation kind, the field `col5` adds (0, or `pg_attribute.attgenerated`'s `s` or `v` code), then the remaining column fields; hexadecimal column name and NOT NULL constraint name (empty when nullable). Extra fields are rejected. |
| `xdep <hex>` | One [expression dependency](#expression-dependencies) of the preceding table, in its journal encoding; at most 64 per table. Index owners are rejected. |
| `attributes2` | Entry count and each attribute entry as sixteen hex digits. |
| `implicit_index POSITION CREATED_AT` | Constraint-created index identities, including the current TOAST index. |
| `inline_key_name ATTRIBUTE HEX_NAME` | One per active inline primary or unique key. |
| `constraint_identity POSITION CREATED_AT` | Table constraint identities in the [canonical order](#constraints-and-keys). |
| `ukey` | Starts with the unique-key mode tag. |
| `dom7`, `enm3`, `cmp2` | Domain, enum and composite types; written before the required `types_end`. Later type records or a missing/duplicate `types_end` are rejected. |
| `nsp` | Creation stamp, database-owner flag (`0`/`1`), hexadecimal name. Stamp zero is an initial public schema. |
| `pub` | Creation stamp after the owner, before the flags. |
| `pacl` | Parameter permissions; an empty row differs from removal. |
| `brtn` | Built-in routine attributes in the preceding `dbctx` database: OID, five attribute codes, cost and rows bits, then the journal record's settings bytes in hex (at most 6,180 characters). `own` and `acl` records for class 20 name the routine OID. |
| `idx` | Index definition; a trailing `xd <count> <hex>...` lists its [expression dependencies](#expression-dependencies) (1–16), omitted when it has none. Table owners are rejected. |
| `ptx` | Prepared transaction; owner as decimal role identifier. |
| `rol` | Role identifier, catalog slot, encoded name and attributes. Never written for the predefined roles, which are bootstrap facts. |
| `rmem` | Position, stamp, role/member/grantor identifiers, option flags. Every live membership is listed, so restoring a manifest first removes the initdb memberships a fresh catalog starts with. |
| `dacl` | Position, group stamp, owner, schema stamp (`-` global), recipient (`PUBLIC` for everyone), class, privileges, grant options. |
| `trg` / `trgs` | Declared trigger position and stamp / inherited stamp, declared-trigger position and stamp, child-table position and stamp, enabled mode. |
| `pol` | Position and creation stamp first. |
| `am`, `cst`, `opr`, `opf`, `opc` | Position before creation stamp. |
| `dsst` | Row SST reference naming its own format (`v2`, `v3`, `v4`). |
| Dependency records | `i2` grammar with a trailing hexadecimal checked-input image (`-` when empty). Earlier grammars are rejected. |

`catalog_sequence` is at least every restored creation stamp, including objects
deleted before checkpoint. `role_sequence` keeps the highest role reservation,
including deleted and rolled-back roles. A full `checkpoint_manifest_bytes`
buffer fails with a named capacity error rather than growing or truncating.

### Row SSTs and blocks

Block headers carry the block type. A row SST format defines the allowed
index-entry shape and block types together; the in-memory handle keeps this as
`RowSstFormat` and never infers a grammar from block contents. `v4` stores PAX
full slices and compressed canonical row groups in verified packed extents, so
small deltas avoid a separate descriptor and column-container write per group.
How this affects reads is described in [architecture](architecture.md).

### Mutable roots and writer fencing

Each process creates a random nonzero 128-bit writer token at startup. It reads
and validates both mutable roots (manifest and commit head) before touching the
writer fence, so an invalid format cannot displace the existing writer. Then it:

1. CAS-publishes a transitioning `pos3ql-writer-fence-v1` record;
2. conditionally rewrites both roots with its token (missing roots receive
   token-bearing empty identities), which changes their ETags even on providers
   that derive ETags from content;
3. CASes the fence to active. This is the ownership-transfer point.

Commit and checkpoint publication use the root ETag that process loaded; after a
conflict they must prove their token still owns the active fence before retrying.
A request begun before promotion either lands and is preserved by the successor's
rewrite, or fails with a stale ETag. An interrupted transition is completed by
the next startup. A prefix without a writer fence is promoted in place. Startup
keeps the root versions it validated; a changed version stops promotion with a
named error.

Promotion replaces the manifest's 32-digit `writer` line with one of the same
width. That line is charged to `checkpoint_manifest_bytes`; promotion scratch is
one fixed 128-byte startup allocation.

### Backups and exports

A named backup stores exact manifest and commit-head images, then publishes its
completion record last. The backup manifest itself is the retention pin, so an
interrupted creation neither exposes a restorable name nor loses referenced
blocks. Version 2 adds the creation timestamp to the manifest LSN and CRC32C
checksums of both roots. Restore rejects a missing, unknown or mismatched
completion record.

An independent-prefix copy publishes `pos3ql-export-v1` as `export-pending`
before copying. A retry must match the backup name and root checksums. Startup
refuses the destination until both roots and the destination cache reset are
complete. `export-complete` (same body) is published before `export-pending` is
removed, so a lost final response can be adopted.

### Catalog positions and creation stamps

A catalog position is a two-byte slot followed by an eight-byte nonzero creation
stamp, both little-endian (ten bytes). Creation stamps are the durable lifetime
of an object; slots are reusable. Restore checks capacity, database, lifetime and
occupancy and installs the saved stamp directly. It never moves an object to a
free slot or reserves a temporary stamp and overwrites it. Rollback gaps are
therefore preserved, and an exhausted sequence cannot prevent loading existing
objects. Reservations are not reused while the server runs.

Public SQL identifiers (OIDs) are a fixed base plus the creation stamp, so names
and slots never determine them. Every family owns one band in a single registry
(`src/storage/oid_bands.rs`); a compile-time check rejects a band that overlaps
another, starts below PostgreSQL's first user OID (16,384) or passes the
largest signed 32-bit OID, so an OID alone identifies its family. Every stamp is in 1–9,999,999; fresh creation
past that returns `54000` and installs nothing. Recovery rejects zero,
out-of-range, duplicate or unreserved stamps (`22000` where a SQLSTATE applies)
before writer promotion, using fixed 8 KiB bitmaps scanned over bounded windows
rather than collections that grow with catalog churn.

| Object | Identifier |
|---|---|
| Bootstrap administrator role | 10 |
| Predefined roles (catalog slots 1–16) | PostgreSQL 18.6's OIDs (`pg_database_owner` 6171, `pg_monitor` 3373, …) |
| Initdb role memberships of `pg_monitor` | 10,308–10,310; journal and manifest stamps 10,000,000–10,000,002 at positions 0–2 |
| User roles | 16,385–9,999,999 (`54000` on exhaustion) |
| Databases | `CREATE DATABASE ... OID n` accepts any n from 16,384, as PostgreSQL does; automatic OIDs are 10,000,001–19,999,999 |
| Parameter permissions (`pg_parameter_acl`) | 20,000,001–29,999,999 |
| Temporary namespaces (`pg_temp_N`) | 30,000,000 + the smallest number no other live temporary namespace holds |
| Built-in tablespaces | 1663, 1664 |
| Initial `public` schema | 2200 |
| Schemas | 40,000,000 + stamp |
| Routines | 50,000,000 + stamp |
| Triggers / their constraint rows (`CREATE CONSTRAINT TRIGGER`) | 60,000,000 / 70,000,000 + trigger stamp |
| Row-security policies | 80,000,000 + stamp |
| Custom tablespaces | 90,000,000 + stamp |
| Column defaults (`pg_attrdef`) | 100,000,000 + stamp; setting a default or generation expression, or changing its column's type, reserves a new stamp |
| Inherited row triggers / their constraint rows | 110,000,000 / 120,000,000 + clone stamp |
| Domains / enums / composites | 180,000,000 / 200,000,000 / 220,000,000 + stamp |
| Their arrays | 240,000,000 / 260,000,000 / 280,000,000 + stamp |
| Table, materialized-view and view row types (and arrays) | The composite bands above + the relation's stamp |
| Tables, views, sequences, composite backing relations, indexes | 300,000,000 + shared relation stamp |
| TOAST relations | 320,000,000 + owner's stamp |
| TOAST indexes | 340,000,000 + own stamp (rebuilding changes it) |
| Enum labels (`pg_enum.oid`) | 360,000,000 + stamp |
| Constraints | 380,000,000 + stamp |
| Foreign-key internal triggers | 400,000,000 + 4 × constraint stamp + 0–3 (delete action, update action, insert check, update check) |
| `pg_statistic_ext` | 440,000,000 + stamp |
| `pg_extension` | 460,000,000 + stamp |
| `pg_foreign_data_wrapper` | 480,000,000 + stamp |
| `pg_foreign_server` | 500,000,000 + stamp |
| `pg_user_mapping` | 520,000,000 + stamp |
| `pg_publication` | 540,000,000 + stamp |
| `pg_subscription` | 560,000,000 + stamp (unique across databases) |
| Collations | 580,000,000 + stamp |
| Conversions | 620,000,000 + stamp |
| Text-search parsers / templates / dictionaries / configurations | 640,000,000 / 660,000,000 / 680,000,000 / 700,000,000 + stamp |
| Casts / operators / operator families / operator classes | 720,000,000 / 740,000,000 / 760,000,000 / 780,000,000 + stamp |
| Access methods / event triggers | 800,000,000 / 820,000,000 + stamp |
| Default privileges / role memberships | 840,000,000 / 860,000,000 + stamp |
| Rewrite rules | 880,000,000 + stamp; a materialized view's `_RETURN` rule uses the view's own stamp |
| Large objects | Any OID; `lo_create(oid)` accepts the client's choice, as PostgreSQL does |

**User example:** an application saves the OID of `orders`, drops the table and
creates another `orders`. The saved OID cannot identify the replacement, even if
it reuses the same slot. Renaming `public` to `archive` and creating a new
`public` leaves `archive` with 2200.

Identity rules by object family:

- **Relations.** Table creation/drop, sequence definition/value/name changes,
  routine definition/name/drop and materialized-view changes carry the ten-byte
  position; materialized-view creation also carries its backing table's position.
- **Schemas.** An explicit owner change is stored separately from the
  `pg_database_owner` default of an initial public schema. Every restored object
  must have a schema in its own database.
- **Comments** on statistics, publications, foreign wrappers and foreign servers
  are keyed by database and creation stamp; PostgreSQL's `objsubid` is unchanged.
- **Role memberships.** One identifier per grant from one grantor; changing ADMIN,
  INHERIT or SET keeps it; revoke-and-regrant reserves a new one. The three
  initdb memberships keep reserved stamps above the catalog creation range,
  which recovery accepts only at their own positions. Grantor
  selection follows PostgreSQL 18.6
  ([`user.c`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/user.c#L2040-L2140)).
  Journal images hold the position, three four-byte identifiers and a flags byte;
  removal holds only the position.
- **Default privileges.** One identifier per owner, schema scope and object
  class. Removing every exception deletes the row; an empty global list differs
  from no row. Grants sort by recipient and grantor identifiers, as in PostgreSQL
  ([`aclchk.c`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/aclchk.c)).
  Journal changes: two-byte position, flags byte, then expected and replacement
  images (four-byte group stamp, four-byte owner, one-byte class, one-byte scope
  flags, optional eight-byte schema stamp, optional four-byte recipient, two bytes
  each for privileges and grant options).
- **Inherited triggers.** Disable and rename keep the identifier;
  detach-and-reattach reserves a new one. The enabled mode is explicit per clone.
  Startup reserves `max_triggers * max_tables` images.
- **Prepared-transaction owners** use the role identifier, not the slot. A
  replacement account cannot finish a deleted account's prepared transaction; a
  superuser still can.
- **Executable catalogs.** Stored index classes, table handlers and operator
  dependencies reject identifiers outside their family range. Operator replay
  keys include database and stamp.
- **Policy, rule and trigger journal records** begin with the ten-byte position.

Catalog slot capacities, including `max_rules`, cannot exceed 65,535.

### Constraints and keys

Primary-key, unique, exclusion, CHECK, foreign-key and NOT NULL constraints,
detached partition CHECKs, and domain CHECK and NOT NULL constraints each have a
creation identity. Renames preserve it; replacing a backing index changes
`conindid` but not the constraint identifier, comments or dependencies.

Table payload v20 and domain marker 251 append one four-byte little-endian
identity per constraint and column default. Table order: CHECKs, foreign keys,
live attributes by stable attribute number (NOT NULL, then the default or
generation expression, each when present), detached partition CHECK, named
primary and unique keys, exclusions, inline keys by attribute number. A column
default's identity selects its `pg_attrdef` OID. Domain order:
CHECKs, then NOT NULL. `dom7` appends domain identities. Positions describe the
layout only. Recovery also rejects identities created before their owner, above
the saved counter, or shared by distinct tables or domains in one database.

| Rule | Format |
|---|---|
| Per-kind capacity | 64 table constraints per kind, 64 domain checks; journal counts are bytes; the 65th returns `54000` |
| Unique-key mode | Tag `0` NULLs distinct, `1` primary key, `2` `NULLS NOT DISTINCT`; other tags rejected. v20 stores tag, key-column count and timing as three bytes before positions |
| Constraint-created indexes | Own stamp; stored beside stable attribute numbers within the 512-byte column metadata bound |
| Inline key names | Chosen once at admission (63-byte limit, no split UTF-8, collision suffix); v20 stores two-byte attribute number plus bounded name per key |
| NOT NULL names | Stored for columns and domains; renaming `orders` keeps `orders_id_not_null` |
| Deferred uniqueness | Stored; every current index-option section is required |

### Expression dependencies

The objects a CHECK constraint, column default or generation expression, or
index key or predicate was bound to when written. Table payloads append a
one-byte count and the entries after the constraint identities; index journal
records append marker `0xa7`, a one-byte count and the entries after the
definition. Each entry:

| Field | Encoding |
|---|---|
| Owner | `1` CHECK constraint, `2` column (each followed by a length-prefixed name), `3` index key (followed by its one-byte position), `4` index predicate |
| Class | One byte, as in stored-query dependencies |
| Identity | Length-prefixed key as stored-query dependencies write it (routine, operator and text search configuration OIDs, type and collation identities) |
| Names | Four length-prefixed names: current schema and name, then the spelling the expression used |

An entry is at most 512 bytes; the slot is bound again on use. Unknown owner
codes, classes or identity keys are rejected.

### Indexes

Index lifecycle codes retain PostgreSQL's `indisvalid`, `indisready`,
`indislive` flags:

| Code | Meaning | Flags |
|---:|---|---|
| 0 | Ordinary | true, true, true |
| 1, 2 | Partitioned index states | — |
| 3 | Concurrent build, unbuilt | false, false, true |
| 4 | Concurrent build, ready | false, true, true |
| 5 | Dead | false, false, false |

Invalid indexes cannot serve reads; ready indexes participate in writes and
uniqueness. TOAST index sources are encoded in 11 journal bytes: one tag plus a
ten-byte owner identity (the ordinary-table tag requires ten zero bytes). They
distinguish real columns from virtual `chunk_id`/`chunk_seq` keys. Unknown tags,
stale owners and contradictory keys are rejected.

### Table layout

The attribute layout maps PostgreSQL column numbers to row positions. Dropped
entries keep length, passing convention, alignment, storage and compression; new
columns append. The journal stores a count and eight bytes per entry. The
1,600-column limit counts dropped entries.

Catalog vectors store their signed lower bound (four bytes each) in rows,
projected values and constant defaults. SQL array-to-vector casts are rejected.

### Types

Every stored user-type name has a prefix; manifests hex-encode these fields.

| Prefix | Meaning |
|---|---|
| `r:<kind>:<slot>:<creation>:<name>` | Bound type; kind `d` domain, `e` enum, `c` composite |
| `n:<name>` | Declaration requiring binding |
| `p:<name>` | Validated PostgreSQL polymorphic type |
| `t:<creation>:<name>` | Row type of the table, materialized view or view created at that stamp; the relation, not the type catalog, owns it |

Typed tables save their composite as `c:<slot>:<creation>`. Database copies bind
it to the copied type. At `types_end`, recovery resolves domain chains and
composite fields before loading dependents. Routine type references refresh their
SQL spelling from the saved source.

Type modifiers: `bit(6)` stores `6`; `varchar(7)` stores `11` (including the
four-byte header).

**Enum members.** A member's position in its type's member list is its stable
identity; adding a member appends even when it sorts BEFORE another. Row values
store that identity separately from the cached label and sort position.
`enm3` and kind 149 write each label's creation stamp after its sort bits and
optional legacy label. Recovery requires increasing stamps, unchanged prefixes,
unique labels and sort keys, and sort keys exactly representable as `float4`.
Labels follow PostgreSQL's
[63-byte limit](https://www.postgresql.org/docs/18/datatype-enum.html#DATATYPE-ENUM-IMPLEMENTATION-DETAILS).

| Encoding | Rule |
|---|---|
| Row enum value | High bit of the u32 label length marks a following u32 member identity |
| Composite field count | Bit 15 marks enum leaves holding member ordinals (nested records and arrays included) |
| Frozen enum default | Tag 201 (member identity) or 202 (label binding), each with a four-byte type stamp; tags 12/42 rejected |
| Cached array default | Tag 200 with a six-byte element descriptor; tags 21/41 rejected |
| Spill values | Arrays use the six-byte descriptor; named scalars carry a four-byte stamp |
| Object-reference default | Full type and target OIDs; at most 130 bytes including a 120-byte cached name; unknown type OIDs fail (set per [PostgreSQL 18](https://www.postgresql.org/docs/18/datatype-oid.html)) |
| Enum indexes | Key hashes use member identities; label-hashed index generations are retired and replaced |

### Saved SQL and values

| Format | Rule |
|---|---|
| `pos3ql-catalog-expression-v1` | Retained CHECK, list, partition-bound and SQL routine-body nodes with source spans and bound column numbers; rendered via `pg_get_expr`. Not PostgreSQL's `pg_node_tree`. |
| `pos3ql-bound-query-v1` | View SQL with its written spacing and comments dropped; `/*p3e:D:M*/` before each typed enum literal binds dependency D, member M, and a literal the query did not cast is followed by a cast to its type. Unknown versions and invalid bindings fail. |
| `pos3ql-enum-check-v1` | Domain CHECK/DEFAULT on enum bases: `/*p3c:M*/` member, `/*p3a*/` member array, `/*p3t:N*/` N-token cast type, `/*p3o:TEXT*/` original default text (`%25`, `%2f` escaped). |
| Stored-query dependencies | Marker `0xfc`, length-prefixed identity, complete read and written column sets, then a four-byte length and checked input image. Types `d:`/`e:`/`c:<slot>:<creation>`; tables and views `t:<creation>`/`v:<creation>` (1–9,999,999). Name-only or wrong-kind keys are rejected. |
| `TIP2` | Checked literal map: four-byte identity, four-byte count, 44-byte records (two eight-byte source offsets, type OID, type modifier, input kind, collation flag, collation code, eight-byte value length) each followed by its value. Ordered by span and kind. `TIP1` rejected. |
| `STI1` | Groups `TIP2` maps by owner fragment: eight-byte header (identity, section count); each section has a two-byte kind, two-byte parameter/attribute number and eight-byte length. Nonempty images charge `table_metadata_bytes`. |
| Record values | Tags `19`/`202` use one-byte counts and name lengths; wide tags `51`/`204` use two bytes; `202`/`204` add a four-byte type stamp. Record array elements use the same tags inside `AR01`. |
| `pg_rewrite` images | Nine typed fields: six public, `tableoid`, private view definition text, private text array (condition, then actions). |

Saved input images keep the typed result of domain checks, so description,
execution and recovery do not rerun user-defined checks.

### Text comparison

A named collation is a four-byte little-endian code: bit 31 marks it named,
bits 2–30 hold the creation identity, bit 0 selects the database locale and
bit 1 full Unicode case mapping (not both). Built-in codes are 0–4 and 133.
Old slot codes and unknown bits are rejected. Collation dependencies are stored
as `c` plus the decimal code.

### Prepared transactions

**Catalog rows (kind 155)** keep the pre-transaction values of changed rows:
32-bit database, relation, key-length, multiplicity and row-length fields, then
key and row bytes. Database zero is a shared catalog; zero multiplicity and
length mean the row did not exist.

**Locks** start with a kind byte:

| Kind | Object | Notes |
|---:|---|---|
| 0 | Table | Mode and qualified name |
| 1 | Row | |
| 2 | Advisory lock | |
| 3 | View | Mode and qualified name |
| 4 | Account | Role identifier in the eight-byte identity field; empty names. Modes 0 (reference), 3 (membership edit), 7 (deletion); bootstrap account allows only mode 3 |

Zero, reserved or above-`i32::MAX` identifiers, nonempty names, unknown kinds and
invalid modes stop recovery. Startup restores prepared records as private
changes with normal undo; `COMMIT PREPARED` reserves its owners before the
decision becomes durable.

### Database copy inventory

Database creation records carry a four-byte count and eleven bytes per copied
object: a one-byte kind and the ten-byte position, ordered by kind and slot.
Unknown kinds, duplicate slots, missing lifetimes, unavailable positions and
unused entries are errors.

| Kinds | Objects |
|---|---|
| 0–8 | Table, domain, enum, composite, sequence, view, rewrite rule, materialized view, routine |
| 9–15 | Foreign wrapper, foreign server, user mapping, foreign-table binding, statistics, publication, extension |
| 16 / 17 / 18 / 19 | Collations / conversions / user text search / built-in text search (keyed by fixed PostgreSQL OID, stamp zero) |
| 20–25 | Casts, access methods, event triggers, operators, operator families, operator classes |
| 26 | Default privileges, keyed `(group_stamp << 32) \| recipient_oid` (zero for PUBLIC) |

Copies keep database-local public identifiers and remap wrapper, server and
foreign-table parents. Shared subscriptions and their comments are not copied.

### Fixed counters

| Counter | Limit and persistence |
|---|---|
| Row identity | `u64::MAX` is terminal; `54000` (`row identities exhausted`). Kept in `next_rowid`; gaps are never reused |
| Transaction identity | 32-bit; `54000` before reuse. Kept in `latest_transaction_id` and journal commits |
| Sequence restart | Presence byte plus optional eight-byte value |

Sequence and serial positions are restored from saved generator state, not
inferred from row values.

### Fixed-memory accounting

All added fields are charged by the common journal sizing function or the
configured manifest buffer; none adds runtime heap growth. Manifest records grow
by at most 22 bytes per table, 27 per sequence, 7 per routine and 54 per
materialized view. Table header scratch is 278 bytes.
