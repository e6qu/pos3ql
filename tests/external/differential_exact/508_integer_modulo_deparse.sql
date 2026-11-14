-- `%` has no cross-type integer operators, so the narrower operand prints
-- with a cast to the wider type.
CREATE TABLE im_t (s smallint, b bigint, i int);
CREATE VIEW im_v AS SELECT s % 3 AS m, s % s AS m2, b % 2 AS w, i % b AS x, 7 % s AS y,
  pg_typeof(s % 3) AS t FROM im_t;
SELECT pg_get_viewdef('im_v');
INSERT INTO im_t VALUES (7, 9, -5);
SELECT * FROM im_v;
DROP TABLE im_t CASCADE;
