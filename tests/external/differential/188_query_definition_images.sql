-- Query shape and record metadata remain PostgreSQL-compatible through
-- alias reuse, nested scopes, transaction-visible DDL, and statement reuse.
CREATE TABLE definition_scope_rows (id integer, value varchar(3));
INSERT INTO definition_scope_rows VALUES (1, 'abc');
SELECT (r).* FROM definition_scope_rows r;
SELECT a.id, b.value FROM definition_scope_rows a CROSS JOIN definition_scope_rows b;
SELECT t IN (SELECT r FROM definition_scope_rows r),
       t NOT IN (SELECT r FROM definition_scope_rows r WHERE r.id = 9)
  FROM definition_scope_rows t;
SELECT a.* FROM definition_scope_rows a JOIN definition_scope_rows b USING (id);
SELECT * FROM definition_scope_rows a JOIN definition_scope_rows b USING (missing);
SELECT * FROM definition_scope_rows a CROSS JOIN definition_scope_rows a;
SELECT * FROM definition_scope_rows a(one, two, three);
SELECT (r).* FROM definition_scope_rows r;
BEGIN;
ALTER TABLE definition_scope_rows ADD COLUMN extra integer DEFAULT 7;
SELECT (r).* FROM definition_scope_rows r;
ROLLBACK;
SELECT (r).* FROM definition_scope_rows r;
ALTER TABLE definition_scope_rows RENAME COLUMN value TO label;
SELECT (r).* FROM definition_scope_rows r;
DROP TABLE definition_scope_rows;
