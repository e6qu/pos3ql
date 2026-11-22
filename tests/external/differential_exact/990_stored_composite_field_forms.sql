-- A composite column stores a whole row or ROW(...) over a stored row with
-- its compressed fields still compressed, so pg_column_size of the stored
-- composite and of its fields counts them as stored; an update of another
-- column, a copy into another table and ANALYZE keep them. A nested
-- composite, an array of composites and a dropped attribute keep them too.
CREATE TABLE cw (id int, a text, b text);
INSERT INTO cw VALUES (1, repeat('a', 3000), 'short'), (2, 'tiny', 'b'), (3, repeat('ab', 1500), repeat('c', 200));
CREATE TABLE cs (id int, x cw, y text);
INSERT INTO cs SELECT id, cw, 'z' FROM cw;
INSERT INTO cs SELECT id + 10, ROW(id, a, b)::cw, 'z' FROM cw;
SELECT id, pg_column_size(x), pg_column_size((x).a), pg_column_size((x).b), length((x).a), pg_column_size(cs) FROM cs ORDER BY cs.id;
SELECT cs.id, (x).id, md5((x).a), (x).b = (SELECT b FROM cw WHERE cw.id = (cs.x).id) FROM cs ORDER BY cs.id;
UPDATE cs SET y = 'zz' WHERE id = 1;
SELECT id, pg_column_size(x), pg_column_size((x).a), y FROM cs WHERE id = 1;
CREATE TABLE cs2 AS SELECT * FROM cs;
SELECT id, pg_column_size(x), pg_column_size((x).a) FROM cs2 ORDER BY id;
INSERT INTO cs2 SELECT id + 100, x, y FROM cs2 WHERE id < 10;
SELECT id, pg_column_size(x), pg_column_size((x).a) FROM cs2 WHERE id > 100 ORDER BY id;
ANALYZE cs;
SELECT attname, avg_width FROM pg_stats WHERE tablename = 'cs' ORDER BY attname;
CREATE TYPE outer_cw AS (tag int, inner_row cw);
CREATE TABLE nest (id int, o outer_cw);
INSERT INTO nest SELECT id, ROW(id, cw)::outer_cw FROM cw;
SELECT id, pg_column_size(o), pg_column_size((o).inner_row), pg_column_size(((o).inner_row).a) FROM nest ORDER BY id;
CREATE TABLE arr (id int, v cw[]);
INSERT INTO arr SELECT 1, ARRAY[c1, c2, c3] FROM cw c1, cw c2, cw c3 WHERE c1.id = 1 AND c2.id = 2 AND c3.id = 3;
SELECT id, pg_column_size(v), pg_column_size(v[1]), pg_column_size(v[3]) FROM arr;
CREATE TYPE named_cw AS (id int, a text, b text);
CREATE TABLE ns (id int, x named_cw);
INSERT INTO ns SELECT id, ROW(id, a, b)::named_cw FROM cw;
ALTER TYPE named_cw DROP ATTRIBUTE b;
SELECT id, pg_column_size(x), pg_column_size((x).a), (x).a = repeat('a', 3000) FROM ns ORDER BY id;
