-- In a SQL function, p.f names field f of composite parameter p unless a FROM
-- item p has a column f; fn.p.f qualifies the parameter by the function name.
CREATE TYPE cp_pair AS (id integer, note text);
CREATE TABLE cp_rows(id integer, note text, extra integer);
INSERT INTO cp_rows VALUES (1, 'row', 10);
CREATE FUNCTION cp_note(o cp_pair) RETURNS text LANGUAGE sql AS 'SELECT o.note';
SELECT cp_note(ROW(1, 'param')::cp_pair);
CREATE FUNCTION cp_qualified(o cp_pair) RETURNS text LANGUAGE sql AS 'SELECT cp_qualified.o.note';
SELECT cp_qualified(ROW(1, 'qualified')::cp_pair);
CREATE FUNCTION cp_shadow(o cp_pair) RETURNS text LANGUAGE sql AS 'SELECT o.note FROM cp_rows o';
SELECT cp_shadow(ROW(1, 'param')::cp_pair);
CREATE FUNCTION cp_column(note cp_pair) RETURNS text LANGUAGE sql AS 'SELECT note FROM cp_rows';
SELECT cp_column(ROW(1, 'param')::cp_pair);
CREATE FUNCTION cp_mixed(o cp_pair) RETURNS text LANGUAGE sql AS 'SELECT o.note || r.note FROM cp_rows r WHERE r.id = o.id';
SELECT cp_mixed(ROW(1, 'param')::cp_pair);
CREATE FUNCTION cp_where(o cp_pair) RETURNS integer LANGUAGE sql AS 'SELECT extra FROM cp_rows WHERE id = o.id';
SELECT cp_where(ROW(1, 'x')::cp_pair);
CREATE FUNCTION cp_atomic(o cp_pair) RETURNS text LANGUAGE sql BEGIN ATOMIC SELECT o.note; END;
SELECT cp_atomic(ROW(1, 'atomic')::cp_pair);
CREATE FUNCTION cp_return(o cp_pair) RETURNS text LANGUAGE sql RETURN o.note || '!';
SELECT cp_return(ROW(1, 'ret')::cp_pair);
CREATE FUNCTION cp_table(o cp_rows) RETURNS integer LANGUAGE sql AS 'SELECT o.extra + 1';
SELECT cp_table(r) FROM cp_rows r;
CREATE FUNCTION cp_missing(o cp_pair) RETURNS text LANGUAGE sql AS 'SELECT o.nothing';
CREATE FUNCTION cp_pos(cp_pair) RETURNS text LANGUAGE sql AS 'SELECT ($1).note';
SELECT cp_pos(ROW(1, 'pos')::cp_pair);
DROP FUNCTION cp_note, cp_qualified, cp_shadow, cp_column, cp_mixed, cp_where, cp_atomic, cp_return, cp_table, cp_pos;
DROP TABLE cp_rows;
DROP TYPE cp_pair;
