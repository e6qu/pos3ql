-- Extended statistics on columns past the 63rd: the planner's masks of
-- columns are wide enough for every column of the table.
CREATE TABLE w896 (c1 integer, c2 integer, c3 integer, c4 integer, c5 integer, c6 integer, c7 integer, c8 integer, c9 integer, c10 integer, c11 integer, c12 integer, c13 integer, c14 integer, c15 integer, c16 integer, c17 integer, c18 integer, c19 integer, c20 integer, c21 integer, c22 integer, c23 integer, c24 integer, c25 integer, c26 integer, c27 integer, c28 integer, c29 integer, c30 integer, c31 integer, c32 integer, c33 integer, c34 integer, c35 integer, c36 integer, c37 integer, c38 integer, c39 integer, c40 integer, c41 integer, c42 integer, c43 integer, c44 integer, c45 integer, c46 integer, c47 integer, c48 integer, c49 integer, c50 integer, c51 integer, c52 integer, c53 integer, c54 integer, c55 integer, c56 integer, c57 integer, c58 integer, c59 integer, c60 integer, c61 integer, c62 integer, c63 integer, c64 integer, c65 integer, c66 integer, c67 integer, c68 integer, c69 integer, c70 integer);
INSERT INTO w896 SELECT g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g, g % 20, g % 20 + (g % 3), g % 5, g % 7, g % 3 , g % 11, g % 13, g % 2, g % 4, g % 6 FROM generate_series(1, 600) g;
CREATE STATISTICS s896 (dependencies, mcv, ndistinct) ON c61, c62, c64 FROM w896;
CREATE STATISTICS s896b (mcv) ON c65, c70 FROM w896;
ANALYZE w896;
EXPLAIN SELECT c1 FROM w896 WHERE c61 = 5 AND c62 = 5;
EXPLAIN SELECT c1 FROM w896 WHERE c61 = 5 AND c62 = 5 AND c64 = 0;
EXPLAIN SELECT c1 FROM w896 WHERE c61 = 5 AND c62 = 6;
EXPLAIN SELECT c1 FROM w896 WHERE c61 < 5 AND c62 > 3;
EXPLAIN SELECT c1 FROM w896 WHERE c61 = 5 OR c62 = 6;
EXPLAIN SELECT c1 FROM w896 WHERE (c61 = 5 AND c62 = 5) OR c64 = 1;
EXPLAIN SELECT c1 FROM w896 WHERE c65 = 1 AND c70 = 1;
EXPLAIN SELECT c1 FROM w896 WHERE c65 = 1 AND c70 = 0 AND c61 = 3;
EXPLAIN SELECT c61, c62, c64 FROM w896 GROUP BY c61, c62, c64;
EXPLAIN SELECT c61, c62 FROM w896 GROUP BY c61, c62;
EXPLAIN SELECT c65, c70 FROM w896 GROUP BY c65, c70;
SELECT count(*) FROM w896 WHERE c61 = 5 AND c62 = 5;
