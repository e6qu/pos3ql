# Backup and restore

pos3ql can retain a named, checkpoint-consistent recovery point inside the
configured object-store prefix. The operation copies the published manifest
and commit head under `backups/<name>/` and publishes a checksummed completion
record last.

Run every backup operation with the server stopped. Writer fencing is still a
roadmap item, so the command cannot evict or fence a process that continues to
use the same prefix.

```sh
pos3ql --config /path/to/pos3ql.conf --backup before-upgrade
pos3ql --config /path/to/pos3ql.conf --restore before-upgrade
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
finishes. Before removing the marker, restore deletes `journal.wal`,
`block-cache`, and `clean.shutdown`, forcing recovery from object storage.
Other operator-managed files below `data_dir`, including extension packages,
are preserved. A successful restore can accept new commits and checkpoints,
forming a new history from the restored point.

`--delete-backup` removes the completion record first and the manifest
retention pin last. Later checkpoint maintenance may then reclaim objects used
only by that backup.

These named backups share the database's bucket and prefix. They protect a
historical point from ordinary checkpoint cleanup and support exact restore,
but they do not protect against loss or deletion of that object-store prefix.
Independent-prefix export and recovery to a target between named checkpoints
remain in [PLAN.md](../PLAN.md).
