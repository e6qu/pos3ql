-- CTE materialization is an evaluation contract, not a planner-only hint.
-- Reused and volatile bodies execute once; NOT MATERIALIZED is effective only
-- for side-effect-free queries, as in PostgreSQL 18.

CREATE SEQUENCE cte_semantics_sequence;
WITH value AS (SELECT nextval('cte_semantics_sequence') AS n)
SELECT left_value.n, right_value.n
FROM value AS left_value CROSS JOIN value AS right_value;

WITH value AS MATERIALIZED (SELECT nextval('cte_semantics_sequence') AS n)
SELECT left_value.n, right_value.n
FROM value AS left_value CROSS JOIN value AS right_value;

WITH value AS NOT MATERIALIZED (SELECT nextval('cte_semantics_sequence') AS n)
SELECT left_value.n, right_value.n
FROM value AS left_value CROSS JOIN value AS right_value;

-- An unreferenced SELECT CTE is not evaluated, even when MATERIALIZED was
-- written. The following nextval therefore advances from 3 to 4.
WITH unused AS MATERIALIZED (SELECT nextval('cte_semantics_sequence'))
SELECT 42;
SELECT nextval('cte_semantics_sequence');

CREATE TABLE cte_semantics_source (id integer);
INSERT INTO cte_semantics_source VALUES (1), (2), (3);

WITH value AS NOT MATERIALIZED (
  SELECT id FROM cte_semantics_source WHERE id <= 2
)
SELECT left_value.id, right_value.id
FROM value AS left_value
JOIN value AS right_value ON left_value.id = right_value.id
ORDER BY 1;

CREATE TABLE cte_semantics_copy AS
WITH value AS MATERIALIZED (
  SELECT nextval('cte_semantics_sequence') AS sequence_value, id
  FROM cte_semantics_source ORDER BY id
)
SELECT sequence_value, id FROM value;
SELECT sequence_value, id FROM cte_semantics_copy ORDER BY id;

CREATE TABLE cte_semantics_dml (sequence_value bigint);
WITH value AS MATERIALIZED (
  SELECT nextval('cte_semantics_sequence') AS n
)
INSERT INTO cte_semantics_dml
SELECT left_value.n
FROM value AS left_value CROSS JOIN value AS right_value
RETURNING sequence_value;

CREATE MATERIALIZED VIEW cte_semantics_matview AS
WITH value AS MATERIALIZED (
  SELECT nextval('cte_semantics_sequence') AS sequence_value, id
  FROM cte_semantics_source ORDER BY id
)
SELECT sequence_value, id FROM value;
SELECT sequence_value, id FROM cte_semantics_matview ORDER BY id;
REFRESH MATERIALIZED VIEW cte_semantics_matview;
SELECT sequence_value, id FROM cte_semantics_matview ORDER BY id;

CREATE VIEW cte_semantics_view AS
WITH value AS MATERIALIZED (
  SELECT nextval('cte_semantics_sequence') AS n
)
SELECT left_value.n AS left_n, right_value.n AS right_n
FROM value AS left_value CROSS JOIN value AS right_value;
SELECT left_n, right_n FROM cte_semantics_view;
SELECT left_n, right_n FROM cte_semantics_view;

-- Query-local WITH scopes retain outer bindings, and a local name shadows an
-- outer CTE without changing evaluate-once behavior.
WITH outer_value AS MATERIALIZED (
  SELECT nextval('cte_semantics_sequence') AS n
)
SELECT nested.left_n, nested.right_n
FROM (
  WITH inner_value AS MATERIALIZED (
    SELECT n FROM outer_value
  )
  SELECT left_value.n AS left_n, right_value.n AS right_n
  FROM inner_value AS left_value CROSS JOIN inner_value AS right_value
) AS nested;

WITH value AS (SELECT 999::bigint AS n)
SELECT (
  WITH value AS MATERIALIZED (
    SELECT nextval('cte_semantics_sequence') AS n
  )
  SELECT left_value.n
  FROM value AS left_value CROSS JOIN value AS right_value
);

WITH RECURSIVE numbers(n) AS (
  SELECT 1
  UNION ALL
  SELECT n + 1 FROM numbers WHERE n < 6
  ORDER BY 1 DESC LIMIT 3 OFFSET 1
)
SELECT n FROM numbers;

BEGIN;
DECLARE cte_semantics_cursor CURSOR FOR
WITH value AS MATERIALIZED (
  SELECT nextval('cte_semantics_sequence') AS n
)
SELECT left_value.n AS left_n, right_value.n AS right_n
FROM value AS left_value CROSS JOIN value AS right_value;
FETCH FORWARD 1 FROM cte_semantics_cursor;
FETCH FORWARD 1 FROM cte_semantics_cursor;
COMMIT;

COPY (
  WITH value AS MATERIALIZED (
    SELECT id, id * 10 AS scaled FROM cte_semantics_source
  )
  SELECT id, scaled FROM value ORDER BY id
) TO STDOUT;

DROP TABLE cte_semantics_copy;
DROP TABLE cte_semantics_dml;
DROP MATERIALIZED VIEW cte_semantics_matview;
DROP VIEW cte_semantics_view;
DROP TABLE cte_semantics_source;
DROP SEQUENCE cte_semantics_sequence;
