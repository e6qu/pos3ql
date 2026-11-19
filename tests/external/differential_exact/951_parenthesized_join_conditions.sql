-- An inner parenthesized join's outer condition joins as soon as the
-- relations it names have joined, so pos3ql's own executor hashes it
-- rather than looping over every row combination.
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
SELECT count(*) FROM na JOIN (nb JOIN nc ON nb.id = nc.id JOIN nd ON nd.id = nc.id)
  ON na.id = nd.id AND nb.k = na.k % 30 WHERE na.k + 0 = na.k;
SELECT count(*) FROM nd LEFT JOIN na ON na.id = nd.id + 300
  JOIN (nb JOIN nc ON nb.id = nc.id) ON nb.id = nd.id AND (na.k IS NULL OR na.k >= 0)
  WHERE nd.k + 0 = nd.k;
SELECT count(*) FROM na JOIN (nb JOIN nc ON nb.id = nc.id) ON na.id = nb.id
  AND EXISTS (SELECT 1 FROM nd WHERE nd.id = nc.id) WHERE na.k + 0 = na.k;
RESET statement_timeout;
