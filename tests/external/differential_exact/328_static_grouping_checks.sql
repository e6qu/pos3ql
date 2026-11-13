-- PostgreSQL rejects an ungrouped column while analyzing a query, so a
-- statement that only describes the query fails too.
CREATE SCHEMA static_grouping_checks;
SET search_path = static_grouping_checks, public;
CREATE TABLE e (a int, b int);
CREATE VIEW counted AS SELECT a, count(*) FROM e;
CREATE VIEW regrouped AS SELECT a FROM e GROUP BY b;
CREATE VIEW filtered AS SELECT a FROM e HAVING true;
PREPARE ungrouped AS SELECT a, count(*) FROM e;
EXPLAIN (COSTS OFF) SELECT a FROM e GROUP BY b;
CREATE TABLE copied AS SELECT b, max(a) FROM e GROUP BY a WITH NO DATA;
CREATE VIEW by_ordinal AS SELECT a, count(*) FROM e GROUP BY 1;
CREATE VIEW total AS SELECT count(*) FROM e HAVING count(*) > 1;
CREATE VIEW every_column AS SELECT * FROM e GROUP BY a, b;
SELECT * FROM by_ordinal, total, every_column;
RESET search_path;
DROP SCHEMA static_grouping_checks CASCADE;
