-- TOAST compresses or moves out of line a wide tsvector, path or polygon as
-- PostgreSQL stores it: pglz reads its binary form, and gives up on data
-- with no repeat early on, so these move out of line uncompressed. A cast
-- of a subquery that forms its value, and a trigger's TG_TABLE_NAME, are
-- values the statement forms, not stored datums passed on.
CREATE TABLE gt (id int, p polygon, pa path, v tsvector);
INSERT INTO gt VALUES (1, polygon(300, circle '<(0,0),1>'), path(polygon(300, circle '<(0,0),1>')), NULL);
INSERT INTO gt VALUES (2, polygon(200, circle '<(5,-3),2>'), NULL, NULL);
INSERT INTO gt VALUES (3, NULL, ('[' || (SELECT string_agg(format('(%s,%s)', i, i % 7), ',') FROM generate_series(1, 300) i) || ']')::path, NULL);
INSERT INTO gt VALUES (4, ('(' || (SELECT string_agg(format('(%s,%s)', i * 0.5, (i * 37) % 11), ',') FROM generate_series(1, 300) i) || ')')::polygon, NULL, NULL);
INSERT INTO gt VALUES (5, NULL, NULL, (SELECT string_agg(format('word%s:1,2,3,4', lpad(i::text, 4, '0')), ' ') FROM generate_series(0, 599) i)::tsvector);
INSERT INTO gt VALUES (6, NULL, NULL, (SELECT string_agg(md5(i::text) || ':' || (i % 50 + 1) || 'A,' || (i % 70 + 60) || 'C', ' ') FROM generate_series(1, 300) i)::tsvector);
INSERT INTO gt VALUES (7, NULL, NULL, to_tsvector('english', (SELECT string_agg(md5(i::text), ' ') FROM generate_series(1, 400) i)));
INSERT INTO gt VALUES (8, NULL, NULL, (SELECT string_agg(md5(i::text), ' ') FROM generate_series(1, 200) i)::tsvector);
SELECT id, pg_column_size(p), pg_column_size(pa), pg_column_size(v) FROM gt ORDER BY id;
VACUUM gt;
SELECT relpages FROM pg_class WHERE relname = 'gt';
DROP TABLE gt;
CREATE TABLE a_rather_long_audited_table_name (id int);
CREATE TABLE audit_log (op text, tab text);
CREATE FUNCTION audit_row() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO audit_log VALUES (TG_OP, TG_TABLE_NAME); RETURN NULL; END';
CREATE TRIGGER audited AFTER INSERT ON a_rather_long_audited_table_name
  FOR EACH ROW EXECUTE FUNCTION audit_row();
INSERT INTO a_rather_long_audited_table_name VALUES (1);
SELECT op, tab, pg_column_size(op), pg_column_size(tab) FROM audit_log ORDER BY tab;
DROP TABLE a_rather_long_audited_table_name, audit_log;
DROP FUNCTION audit_row();
