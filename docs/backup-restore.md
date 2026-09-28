# Backup and restore

pos3ql can retain a named, checkpoint-consistent recovery point inside the
configured object-store prefix. The operation copies the published manifest
and commit head under `backups/<name>/` and publishes a checksummed completion
record last.

Each backup operation that opens the database promotes its own writer
incarnation. A running server on the same prefix is fenced and its next durable
publish fails with SQLSTATE `40001`. Coordinate these commands with traffic to
avoid disrupting clients; restart the intended server after the operation.

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

`--backup` recovers the latest durable state, completes a checkpoint, then
creates the named recovery point. A name contains 1 to 63 ASCII letters,
digits, dots, underscores, or hyphens. Names are immutable and cannot be
reused until `--delete-backup` removes the old or incomplete backup.
`max_backups` reserves the fixed startup roster used by checkpoint retention;
creation fails before writing a new backup when that roster is full.

Checkpoint maintenance treats every retained backup manifest as a live root.
It walks the backup's row and value-index rosters before deleting blocks, and
commit pruning retains history from the oldest backup replay floor, including
older prepared transactions and logical replication restart positions. An
interrupted creation that reached the manifest therefore remains a retention
pin even though restore rejects it without the completion record.

`--restore` replaces the live manifest and commit head with the named roots.
A durable `restore-pending` marker makes the two-root replacement recoverable:
ordinary server startup refuses the prefix until the offline restore command
finishes. Restore publishes that marker before promotion, then verifies writer
ownership before each root replacement. Before removing the marker, restore deletes `journal.wal`,
`block-cache`, and `clean.shutdown`, forcing recovery from object storage.
Other operator-managed files below `data_dir`, including extension packages,
are preserved. A successful restore can accept new commits and checkpoints,
forming a new history from the restored point.

`--recovery-target-lsn` replays retained history after the named point through
the exact `pg_lsn` transaction boundary. A record LSN inside a transaction,
an LSN before the backup, and an LSN newer than the retained branch are
rejected. `--recovery-target-time` selects the last transaction committed at
or before the supplied PostgreSQL `timestamptz`; include `Z` or a numeric
offset in operational commands. Timestamp recovery requires a backup written
with the current `pos3ql-backup-v2` completion record. Older v1 backups remain
restorable by their named point or an exact LSN.

Recovery validates the retained descriptor chain, immutable batch checksum,
WAL framing, and commit timestamps before publishing `restore-pending`. If the
chosen boundary falls inside a batch, the command publishes a checksummed
immutable prefix linked to the same predecessor. The marker binds the backup,
manifest, resolved LSN, and target head, so a retry cannot adopt another
target. The live commit chain must still descend from the named backup; an
earlier restore that created a different branch makes its discarded future
unavailable.

`--delete-backup` removes the completion record first and the manifest
retention pin last. Later checkpoint maintenance may then reclaim objects used
only by that backup.

`--export-backup` copies the named roots and the immutable block, retained
commit, and durable extension-package namespaces into the destination
configuration's empty prefix.
The source and destination may use different S3-compatible endpoints, buckets,
credentials, or prefixes. The destination `data_dir` must also be distinct.
Its startup capacities must accommodate the exported database; export checks
the manifest byte bound before creating the destination marker.
The command publishes the named point as the destination's live database and
retains the named backup there. Object bodies stream through a fixed buffer in
ranged reads, independent of the configured response window. Extra immutable
source objects may be copied; ordinary destination checkpoint maintenance
reclaims objects outside the exported live and backup roots.

Export writes `export-pending` before the first copied object. Normal startup
refuses that destination until the same export command finishes. A retry scans
the source in fixed `checkpoint_garbage_batch_objects` batches and adopts only
byte-identical destination objects. It publishes the live manifest and commit
head after every immutable object is present, clears the destination's local
journal and block cache, writes an `export-complete` receipt, and removes the
pending marker last. The receipt lets a retry adopt a completed publication if
the final response was lost, provided its live roots have not advanced. The
destination prefix must be empty on the first attempt; this prevents an export
from silently replacing an existing database.

Same-prefix named backups protect historical points and the commit history
needed for LSN and timestamp recovery from ordinary checkpoint cleanup.
Independent exports also protect against loss of the source prefix.
