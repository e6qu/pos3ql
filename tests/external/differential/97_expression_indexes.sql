DROP TABLE IF EXISTS expression_index_rows;
CREATE TABLE expression_index_rows (email text, active boolean);
CREATE UNIQUE INDEX expression_index_email
  ON expression_index_rows ((lower(email)));
INSERT INTO expression_index_rows VALUES ('Alice@example.com', true);
INSERT INTO expression_index_rows VALUES ('alice@example.com', false);
INSERT INTO expression_index_rows VALUES ('ALICE@example.com', false)
  ON CONFLICT ((lower(email))) DO NOTHING;
INSERT INTO expression_index_rows VALUES ('alice@EXAMPLE.com', false)
  ON CONFLICT DO NOTHING;
CREATE UNIQUE INDEX expression_index_active_email
  ON expression_index_rows ((lower(email))) WHERE active;
SELECT email, active FROM expression_index_rows ORDER BY email;
SELECT indexdef FROM pg_indexes
  WHERE tablename = 'expression_index_rows' ORDER BY indexname;
DROP TABLE expression_index_rows;
