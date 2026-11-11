-- UNION ALL returns each query's rows in order, coerced to the combined
-- column types, which follow PostgreSQL's select_common_type: within the
-- string category the running type stays unless only it casts implicitly
-- to the next one, and an untyped literal adopts the other queries' type.
CREATE TABLE union_a(id int, payload text);
CREATE TABLE union_b(id bigint, note varchar(10), code char(3), label name);
INSERT INTO union_a SELECT g, repeat('a', g % 7) FROM generate_series(1, 40) g;
INSERT INTO union_b SELECT g, 'n' || g, 'c' || (g % 10), 'l' || g FROM generate_series(1, 40) g;
SELECT id, payload FROM union_a WHERE id <= 3
UNION ALL SELECT id, note FROM union_b WHERE id > 37
UNION ALL SELECT g, 'gen' FROM generate_series(1, 2) g;
SELECT pg_typeof(id), pg_typeof(payload) FROM
  (SELECT id, payload FROM union_a UNION ALL SELECT id, note FROM union_b) s LIMIT 1;
SELECT pg_typeof(x) FROM (SELECT note x FROM union_b UNION ALL SELECT payload FROM union_a) s LIMIT 1;
SELECT pg_typeof(x) FROM (SELECT note x FROM union_b UNION ALL SELECT code FROM union_b) s LIMIT 1;
SELECT pg_typeof(x) FROM (SELECT code x FROM union_b UNION ALL SELECT note FROM union_b) s LIMIT 1;
SELECT pg_typeof(x) FROM (SELECT label x FROM union_b UNION ALL SELECT payload FROM union_a) s LIMIT 1;
SELECT pg_typeof(x) FROM (SELECT note x FROM union_b UNION ALL SELECT label FROM union_b) s LIMIT 1;
SELECT x FROM (SELECT code x FROM union_b WHERE id = 2 UNION SELECT 'c2'::text) s ORDER BY 1;
SELECT id FROM union_a WHERE id <= 2 UNION ALL SELECT id FROM union_b WHERE id <= 2 LIMIT 3 OFFSET 1;
(SELECT id FROM union_a ORDER BY id DESC LIMIT 2) UNION ALL (SELECT id FROM union_b ORDER BY id LIMIT 2);
SELECT id FROM union_a WHERE id = 1 UNION ALL SELECT '7' FROM union_b WHERE id = 1;
SELECT id FROM union_a WHERE id = 1 UNION ALL SELECT 'x' FROM union_b WHERE id = 1;
SELECT id, payload FROM union_a UNION ALL SELECT id FROM union_b;
SELECT 'a'::char x UNION ALL SELECT 'b'::"char";
-- A failure in a later query follows the earlier rows.
SELECT 10 / (id - 40) FROM union_a WHERE id > 37 UNION ALL SELECT 1 FROM union_b;
SELECT id FROM union_a WHERE id <= 2 UNION ALL SELECT 10 / (id - 2) FROM union_b;
COPY (SELECT id, payload FROM union_a WHERE id <= 2 UNION ALL SELECT id, note FROM union_b WHERE id <= 2) TO STDOUT;
BEGIN;
DECLARE union_cursor CURSOR FOR SELECT id FROM union_a UNION ALL SELECT id FROM union_b;
FETCH 2 FROM union_cursor;
MOVE 38 IN union_cursor;
FETCH 3 FROM union_cursor;
COMMIT;
DROP TABLE union_a, union_b;
