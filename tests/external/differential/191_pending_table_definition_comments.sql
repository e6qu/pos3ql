CREATE SCHEMA pending_comment_moved;
CREATE TABLE pending_comment_target (
  value integer,
  CONSTRAINT pending_comment_positive CHECK (value > 0)
);
COMMENT ON TABLE pending_comment_target IS 'retained table comment';
COMMENT ON CONSTRAINT pending_comment_positive ON pending_comment_target
  IS 'retained constraint comment';
BEGIN;
SAVEPOINT before_move;
ALTER TABLE pending_comment_target SET SCHEMA pending_comment_moved;
SELECT obj_description('pending_comment_moved.pending_comment_target'::regclass);
SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint
  WHERE conrelid = 'pending_comment_moved.pending_comment_target'::regclass
    AND conname = 'pending_comment_positive';
ROLLBACK TO before_move;
SELECT obj_description('pending_comment_target'::regclass);
SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint
  WHERE conrelid = 'pending_comment_target'::regclass
    AND conname = 'pending_comment_positive';
COMMIT;
SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint
  WHERE conrelid = 'pending_comment_target'::regclass
    AND conname = 'pending_comment_positive';
BEGIN;
ALTER TABLE pending_comment_target
  RENAME CONSTRAINT pending_comment_positive TO pending_comment_renamed;
SAVEPOINT before_second_move;
ALTER TABLE pending_comment_target SET SCHEMA pending_comment_moved;
ROLLBACK TO before_second_move;
SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint
  WHERE conrelid = 'pending_comment_target'::regclass
    AND conname = 'pending_comment_renamed';
ROLLBACK;
SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint
  WHERE conrelid = 'pending_comment_target'::regclass
    AND conname = 'pending_comment_positive';
BEGIN;
SAVEPOINT before_failed_move;
ALTER TABLE pending_comment_target SET SCHEMA pending_comment_moved;
ALTER TABLE pending_comment_moved.pending_comment_target ADD COLUMN value integer;
ROLLBACK TO before_failed_move;
SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint
  WHERE conrelid = 'pending_comment_target'::regclass
    AND conname = 'pending_comment_positive';
COMMIT;
DROP TABLE pending_comment_target;
DROP SCHEMA pending_comment_moved;
