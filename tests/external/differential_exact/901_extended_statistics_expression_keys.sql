-- Conditions on two or more keys of a statistics object, some of them
-- expressions, are estimated through its MCV list and functional
-- dependencies (`statext_is_compatible_clause` matches a clause's
-- expression with the object's).
CREATE TABLE y901 (a integer, b text, c integer);
INSERT INTO y901
SELECT g % 40, 'k' || (g % 40 % 8), g % 5 FROM generate_series(1, 4000) g;
CREATE STATISTICS s901 (dependencies, mcv) ON (a % 8), lower(b), c FROM y901;
CREATE STATISTICS s901b (mcv) ON a, (a % 8) FROM y901;
ANALYZE y901;
EXPLAIN SELECT * FROM y901 WHERE a % 8 = 3 AND lower(b) = 'k3';
EXPLAIN SELECT * FROM y901 WHERE a % 8 = 3 AND lower(b) = 'k4';
EXPLAIN SELECT * FROM y901 WHERE a % 8 = 3 AND lower(b) = 'k3' AND c = 3;
EXPLAIN SELECT * FROM y901 WHERE lower(b) = 'k2' AND c = 2;
EXPLAIN SELECT * FROM y901 WHERE a % 8 < 3 AND lower(b) < 'k3';
EXPLAIN SELECT * FROM y901 WHERE a = 11 AND a % 8 = 3;
EXPLAIN SELECT * FROM y901 WHERE a = 11 AND a % 8 = 4;
EXPLAIN SELECT * FROM y901 WHERE (a % 8 = 3 AND lower(b) = 'k3') OR (a % 8 = 1 AND lower(b) = 'k1');
EXPLAIN SELECT * FROM y901 WHERE a % 8 IN (1, 2) AND lower(b) IN ('k1', 'k2');
EXPLAIN SELECT * FROM y901 WHERE a % 8 = 3 AND lower(b) IS NOT NULL;
EXPLAIN SELECT * FROM y901 WHERE a % 8 = 3 AND b = 'k3';
EXPLAIN SELECT * FROM y901 WHERE a % 9 = 3 AND lower(b) = 'k3';
SELECT count(*) FROM y901 WHERE a % 8 = 3 AND lower(b) = 'k3';
