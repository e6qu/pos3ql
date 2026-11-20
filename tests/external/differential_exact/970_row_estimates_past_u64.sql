-- Row estimates past 2^64 print in full from their double, as
-- PostgreSQL's %.0f does, in text and structured formats.
CREATE TABLE er (id int);
INSERT INTO er SELECT g FROM generate_series(1, 1000) g;
ANALYZE er;
EXPLAIN SELECT 1 FROM er e1 CROSS JOIN er e2 CROSS JOIN er e3 CROSS JOIN er e4 CROSS JOIN er e5 CROSS JOIN er e6 CROSS JOIN er e7;
EXPLAIN (FORMAT JSON) SELECT 1 FROM er e1 CROSS JOIN er e2 CROSS JOIN er e3 CROSS JOIN er e4 CROSS JOIN er e5 CROSS JOIN er e6 CROSS JOIN er e7;
EXPLAIN (FORMAT YAML) SELECT count(*) FROM er e1, er e2, er e3, er e4, er e5, er e6, er e7 WHERE e1.id < e7.id;
DROP TABLE er;
