-- Writing through a view that is not automatically updatable fails with
-- PostgreSQL's message, DETAIL naming the first reason its rewriter finds
-- and HINT naming the trigger or rule that would make the write work.
CREATE SCHEMA view_write_errors;
SET search_path = view_write_errors, public;
CREATE TABLE t (a int, b int);
CREATE VIEW constant_row AS SELECT 1 AS a;
CREATE VIEW distinct_rows AS SELECT DISTINCT a FROM t;
CREATE VIEW grouped AS SELECT a FROM t GROUP BY a;
CREATE VIEW limited AS SELECT a, b FROM t LIMIT 1;
CREATE VIEW counted AS SELECT count(*) FROM t;
CREATE VIEW combined AS SELECT a FROM t UNION SELECT b FROM t;
CREATE VIEW with_query AS WITH w AS (SELECT a FROM t) SELECT a FROM w;
CREATE VIEW numbered AS SELECT a, row_number() OVER () FROM t;
CREATE VIEW expanded AS SELECT a, generate_series(1, 2) FROM t;
CREATE VIEW joined AS SELECT t.a FROM t, t t2;
CREATE VIEW computed AS SELECT a + 1 AS c FROM t;
INSERT INTO constant_row VALUES (1);
UPDATE constant_row SET a = 1;
DELETE FROM constant_row;
INSERT INTO distinct_rows VALUES (1);
DELETE FROM distinct_rows;
INSERT INTO grouped VALUES (1);
INSERT INTO limited VALUES (1, 2);
INSERT INTO counted VALUES (1);
UPDATE counted SET count = 1;
INSERT INTO combined VALUES (1);
INSERT INTO with_query VALUES (1);
INSERT INTO numbered VALUES (1);
INSERT INTO expanded VALUES (1);
DELETE FROM joined;
INSERT INTO computed VALUES (1);
UPDATE computed SET c = 1;
MERGE INTO constant_row USING t ON true WHEN MATCHED THEN DELETE;
MERGE INTO constant_row USING t ON true
  WHEN NOT MATCHED THEN DO NOTHING
  WHEN MATCHED THEN UPDATE SET a = 2;
MERGE INTO constant_row USING t ON true WHEN NOT MATCHED THEN INSERT VALUES (1);
CREATE VIEW checked AS SELECT DISTINCT a FROM t WITH CHECK OPTION;
CREATE VIEW unchecked AS SELECT a FROM t GROUP BY a;
ALTER VIEW unchecked SET (check_option = local);
SELECT table_name, is_updatable, is_insertable_into
  FROM information_schema.views
 WHERE table_schema = 'view_write_errors' ORDER BY 1;
RESET search_path;
DROP SCHEMA view_write_errors CASCADE;
