-- A statement passes a stored value on as the stored datum: INSERT ...
-- SELECT, CREATE TABLE AS, derived tables, sorts, joins, binary-coercible
-- casts, COALESCE and CASE keep the form TOAST stored it in, so a value
-- compressed in a wide row stays compressed in a narrow one. A value saved
-- out of line is fetched back and stored anew; a value an expression forms
-- is formed anew.
CREATE TABLE src (id int, a text, b text);
INSERT INTO src VALUES (1, repeat('y', 1500), repeat('y', 1500));
INSERT INTO src VALUES (2, repeat('x', 3000), 'short');
INSERT INTO src SELECT 3, string_agg(md5(i::text), ''), NULL FROM generate_series(1, 300) i;
SELECT id, pg_column_size(a), pg_column_size(b) FROM src ORDER BY id;
CREATE TABLE dst (id int, a text);
INSERT INTO dst SELECT id, a FROM src;
INSERT INTO dst SELECT id + 10, a || '' FROM src;
INSERT INTO dst SELECT id + 20, a FROM (SELECT id, a FROM src ORDER BY id DESC) s;
INSERT INTO dst SELECT s.id + 30, t.a FROM src s JOIN src t ON t.id = s.id;
INSERT INTO dst SELECT id + 40, coalesce(a, 'z') FROM src;
INSERT INTO dst SELECT id + 50, CASE WHEN id > 1 THEN a ELSE b END FROM src;
SELECT id, pg_column_size(a) FROM dst ORDER BY id;
SELECT id, pg_column_size(a), pg_column_size(a::varchar), pg_column_size(coalesce(b, a))
  FROM src ORDER BY id;
SELECT id, pg_column_size(a) FROM (SELECT id, a FROM src ORDER BY id DESC) s ORDER BY id;
CREATE TABLE made AS SELECT id, a FROM src;
SELECT id, pg_column_size(a) FROM made ORDER BY id;
UPDATE src SET b = a WHERE id = 1;
UPDATE src SET b = a WHERE id = 3;
SELECT id, pg_column_size(a), pg_column_size(b) FROM src ORDER BY id;
DROP TABLE src, dst, made;
