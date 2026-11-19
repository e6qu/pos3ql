-- With only inner joins, a parenthesized join's outer condition filters the
-- flattened chain like WHERE, so pos3ql's own executor joins by it as soon
-- as its relations have joined rather than looping over every combination.
CREATE TABLE na (id int, k int);
CREATE TABLE nb (id int, k int);
CREATE TABLE nc (id int, k int);
CREATE TABLE nd (id int, k int);
INSERT INTO na SELECT g, g % 40 FROM generate_series(1, 400) g;
INSERT INTO nb SELECT g, g % 30 FROM generate_series(1, 300) g;
INSERT INTO nc SELECT g, g % 20 FROM generate_series(1, 200) g;
INSERT INTO nd SELECT g, g % 10 FROM generate_series(1, 100) g;
ANALYZE na, nb, nc, nd;
SET statement_timeout = '20s';
SELECT count(*), sum(na.k) FROM na JOIN (nb JOIN nc ON nb.id = nc.id) ON na.id = nb.id
  WHERE na.k + 0 = na.k;
SELECT count(*), sum(nc.k) FROM na JOIN (nb JOIN nc ON nb.id = nc.id) ON na.id = nb.id
  AND na.k < 20 WHERE na.k + 0 = na.k;
RESET statement_timeout;
