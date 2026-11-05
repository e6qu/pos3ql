CREATE TABLE savepoint_cursor_rows(id integer);
INSERT INTO savepoint_cursor_rows VALUES(1),(2);
BEGIN;
DECLARE old_cursor NO SCROLL CURSOR FOR SELECT 100/(id-1) FROM savepoint_cursor_rows;
SAVEPOINT cursor_boundary;
DECLARE new_cursor NO SCROLL CURSOR FOR SELECT 100/(id-1) FROM savepoint_cursor_rows;
-- error: 22012
FETCH NEXT FROM new_cursor;
ROLLBACK TO cursor_boundary;
SELECT name FROM pg_cursors ORDER BY name;
DECLARE new_cursor CURSOR FOR SELECT 7;
FETCH NEXT FROM new_cursor;
ROLLBACK TO cursor_boundary;
-- error: 22012
FETCH NEXT FROM old_cursor;
ROLLBACK TO cursor_boundary;
-- error: 55000
FETCH NEXT FROM old_cursor;
ROLLBACK TO cursor_boundary;
CLOSE old_cursor;
ROLLBACK TO cursor_boundary;
SELECT count(*) FROM pg_cursors;
COMMIT;
