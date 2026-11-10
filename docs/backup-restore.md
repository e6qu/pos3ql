# Backup and restore

A **named backup** is a checkpoint-consistent recovery point kept inside the
instance's object-store prefix, covering all its SQL databases. An **export**
copies a backup to a separate prefix, bucket or endpoint, protecting against
loss of the source prefix.

**User example:** before an upgrade, stop traffic and the server, create a
backup named `before-upgrade`, then restart. If the upgrade damages data, stop
the server again and restore that point. Export the backup too if you need to
survive losing the whole source prefix.

Backups preserve saved bytes; they do not convert an incompatible pos3ql
format. For scheduling and migrations, see
[operations](operations.md#scheduled-work-and-schema-migrations).

## Commands

```sh
pos3ql --config /path/to/pos3ql.conf --backup before-upgrade
pos3ql --config /path/to/pos3ql.conf --restore before-upgrade
pos3ql --config /path/to/pos3ql.conf --restore before-upgrade \
  --recovery-target-lsn 0/16B6C50
pos3ql --config /path/to/pos3ql.conf --restore before-upgrade \
  --recovery-target-time '2026-09-28 14:30:00+00'
pos3ql --config /path/to/source.conf --export-backup before-upgrade \
  --destination-config /path/to/destination.conf
pos3ql --config /path/to/pos3ql.conf --delete-backup before-upgrade
```

These are offline commands, not SQL. Each one takes write ownership of the
prefix by promoting its own writer identity, so a server still running on that
prefix is fenced: its next durable publish fails with SQLSTATE `40001`. Stop
traffic and the server first, and restart it afterwards.

## Creating and deleting backups

`--backup` recovers the latest durable state, completes a checkpoint, copies
the published manifest and commit head under `backups/<name>/`, and publishes a
checksummed completion record last.

- A name has 1 to 63 ASCII letters, digits, dots, underscores or hyphens.
- Names are immutable: reuse requires `--delete-backup` first, including for an
  incomplete backup.
- `max_backups` sizes a fixed startup roster; creation fails before writing
  when it is full.

Checkpoint cleanup treats every retained backup manifest as a live root: its
row and index files are kept, and commit history is kept from the oldest
backup's replay floor, including older prepared transactions and logical
replication restart positions. An interrupted backup that reached its manifest
is still kept, even though restore rejects it without a completion record.
Capacity exhaustion stops cleanup before any deletion.

`--delete-backup` removes the completion record first and the manifest last.
Later checkpoint cleanup can then reclaim objects used only by that backup.

## Restoring

`--restore` replaces the live manifest and commit head with the backup's
roots. It first publishes a durable `restore-pending` marker, and ordinary
startup refuses the prefix until the restore command finishes, so an
interrupted restore can be retried safely. Restore checks writer ownership
before each root replacement. Before removing the marker it deletes
`journal.wal`, `block-cache` and `clean.shutdown` from `data_dir`, forcing
recovery from object storage; other files there, such as extension packages,
are kept. The restored database accepts new commits, starting a new history.

Point-in-time recovery replays retained history after the backup:

| Option | Stops at | Rejected when |
|---|---|---|
| `--recovery-target-lsn` | The exact `pg_lsn` transaction boundary. | The LSN is inside a transaction, before the backup, or beyond the retained history. |
| `--recovery-target-time` | The last transaction committed at or before the given `timestamptz`. Include `Z` or a numeric offset. | The backup has only a `pos3ql-backup-v1` completion record; time targets need `pos3ql-backup-v2`. v1 backups restore by name or exact LSN. |

For example, restoring to `14:30:00+00` selects the last retained transaction
committed no later than that time; it never stops partway through a
transaction.

Before publishing `restore-pending`, recovery validates the retained descriptor
chain, batch checksums, journal framing and commit timestamps. If the target
falls inside a batch, it publishes a checksummed prefix of that batch linked to
the same predecessor. The marker binds the backup, manifest, resolved LSN and
target head, so a retry cannot pick a different target.

The live commit chain must still descend from the backup. A backup does not
recreate history already discarded, and after a restore creates a new branch,
the discarded later branch cannot be recovered from it.

## Exporting to another prefix

`--export-backup` copies the backup's roots plus the immutable block, retained
commit and durable extension-package objects into the destination
configuration's prefix. The destination may use a different endpoint, bucket,
credentials or prefix; its `data_dir` must also differ, and its startup
capacities must fit the database (the manifest size is checked before
anything is written). The backup becomes the destination's live database and
also stays there as a named backup.

- The destination prefix must be empty on the first attempt, so an export can
  never replace an existing database.
- Export writes `export-pending` before copying anything; normal startup
  refuses the destination until the same command finishes.
- Objects stream through a fixed buffer using ranged reads and conditional
  writes. A retry scans the source in `checkpoint_garbage_batch_objects`
  batches and adopts only byte-identical destination objects.
- After every object is present, export publishes the live manifest and commit
  head, clears the destination's local journal and block cache, writes an
  `export-complete` receipt and removes the pending marker last. The receipt
  lets a retry recognize a completed export whose final response was lost, as
  long as the destination's roots have not advanced.

Extra immutable source objects may be copied; destination checkpoint cleanup
reclaims anything outside its live and backup roots.
