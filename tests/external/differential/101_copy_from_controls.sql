-- PostgreSQL 18 COPY FROM controls: typed header, default marker, conversion
-- error policy, and text-format delimiter/null decoding share one stream setup.
DROP TABLE IF EXISTS diff_copy_controls;
CREATE TABLE diff_copy_controls (id integer DEFAULT 9, note text);

COPY diff_copy_controls FROM STDIN WITH (
  FORMAT csv,
  HEADER match,
  DEFAULT 'DEFAULT',
  ON_ERROR ignore,
  REJECT_LIMIT 2,
  LOG_VERBOSITY silent
);
id,note
1,one
not-an-integer,discarded
DEFAULT,defaulted
\.

COPY diff_copy_controls FROM STDIN WITH (DELIMITER '|', NULL 'NULL');
2|NULL
\.

SELECT id, note IS NULL, note FROM diff_copy_controls ORDER BY id;
DROP TABLE diff_copy_controls;
