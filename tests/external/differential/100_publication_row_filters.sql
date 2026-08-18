-- Publication row filters: typed DDL and catalog visibility.  pgoutput frame
-- behavior is covered by the raw-wire and in-process replication tests.
DROP PUBLICATION IF EXISTS diff_row_filter_changes;
DROP TABLE IF EXISTS diff_row_filter_source;

CREATE TABLE diff_row_filter_source (id int PRIMARY KEY, payload text);
CREATE PUBLICATION diff_row_filter_changes
  FOR TABLE diff_row_filter_source WHERE (id > 0)
  WITH (publish = 'insert, update, delete');

SELECT prqual IS NOT NULL, prattrs IS NULL
  FROM pg_publication_rel
 WHERE prpubid = (SELECT oid FROM pg_publication WHERE pubname = 'diff_row_filter_changes');

DROP PUBLICATION diff_row_filter_changes;
DROP TABLE diff_row_filter_source;
