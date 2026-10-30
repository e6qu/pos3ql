-- PostgreSQL 18: SPI_cursor_open_internal rejects zero or multiple commands.
-- https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/executor/spi.c
CREATE SEQUENCE cursor_query_calls;
CREATE FUNCTION cursor_query_count(q text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE c refcursor;
BEGIN
  OPEN c FOR EXECUTE q;
  CLOSE c;
  RETURN;
END $$;
SELECT cursor_query_count('SELECT nextval(''cursor_query_calls''); SELECT 2');
SELECT last_value, is_called FROM cursor_query_calls;
SELECT cursor_query_count('');
DROP FUNCTION cursor_query_count(text);
DROP SEQUENCE cursor_query_calls;

CREATE FUNCTION cursor_captured_locals() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'captured_locals'; label text := 'opening value'; d date;
BEGIN
  OPEN c FOR SELECT label AS label, d AS day, found AS was_found;
  label := 'changed';
  d := DATE '2026-10-30';
  RETURN c;
END $$;
BEGIN;
SELECT cursor_captured_locals();
FETCH ALL FROM captured_locals;
CLOSE captured_locals;
COMMIT;
DROP FUNCTION cursor_captured_locals();

CREATE TABLE cursor_retained_orders(id integer, label text);
INSERT INTO cursor_retained_orders VALUES (1,'first'),(2,'second'),(3,'third');
BEGIN;
DECLARE retained_orders NO SCROLL CURSOR FOR SELECT id,label FROM cursor_retained_orders;
FETCH 0 FROM retained_orders;
MOVE 0 FROM retained_orders;
FETCH NEXT FROM retained_orders;
UPDATE cursor_retained_orders SET label='changed' WHERE id=2;
INSERT INTO cursor_retained_orders VALUES (4,'later');
FETCH NEXT FROM retained_orders;
MOVE 0 FROM retained_orders;
FETCH ALL FROM retained_orders;
FETCH NEXT FROM retained_orders;
CLOSE retained_orders;
COMMIT;

CREATE FUNCTION cursor_retained_native() RETURNS text LANGUAGE plpgsql AS $$
DECLARE c refcursor; saved text; n integer; captured_label text := 'opening';
BEGIN
  OPEN c NO SCROLL FOR SELECT id,captured_label FROM cursor_retained_orders;
  captured_label := 'changed';
  FETCH NEXT FROM c INTO n,saved;
  CLOSE c;
  RETURN saved;
END $$;
SELECT cursor_retained_native();
DROP FUNCTION cursor_retained_native();
CREATE INDEX cursor_retained_order_idx ON cursor_retained_orders(id);

-- PostgreSQL CheckTableNotInUse keeps an open reader attached to its table.
-- https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/tablecmds.c
BEGIN;
DECLARE busy_orders NO SCROLL CURSOR FOR SELECT 1 AS visible FROM cursor_retained_orders;
SAVEPOINT busy_ddl;
DROP TABLE cursor_retained_orders;
ROLLBACK TO SAVEPOINT busy_ddl;
TRUNCATE cursor_retained_orders;
ROLLBACK TO SAVEPOINT busy_ddl;
ALTER TABLE cursor_retained_orders ADD COLUMN extra integer;
ROLLBACK TO SAVEPOINT busy_ddl;
CLUSTER cursor_retained_orders USING cursor_retained_order_idx;
ROLLBACK TO SAVEPOINT busy_ddl;
ALTER TABLE cursor_retained_orders RENAME TO cursor_renamed_orders;
FETCH ALL FROM busy_orders;
SAVEPOINT after_fetch;
DROP TABLE cursor_renamed_orders;
ROLLBACK TO SAVEPOINT after_fetch;
CLOSE busy_orders;
ALTER TABLE cursor_renamed_orders ADD COLUMN extra integer;
DROP TABLE cursor_renamed_orders;
COMMIT;

-- PostgreSQL PortalRunFetch marks a cursor failed after an execution error.
-- https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/tcop/pquery.c
CREATE TABLE cursor_fetch_failure(id integer);
INSERT INTO cursor_fetch_failure VALUES(1),(2);
BEGIN;
DECLARE fetch_failure NO SCROLL CURSOR FOR SELECT id FROM cursor_fetch_failure;
FETCH NEXT FROM fetch_failure;
MOVE 0 FROM fetch_failure;
MOVE RELATIVE 0 FROM fetch_failure;
SAVEPOINT before_fetch_error;
FETCH 0 FROM fetch_failure;
ROLLBACK TO SAVEPOINT before_fetch_error;
FETCH NEXT FROM fetch_failure;
ROLLBACK TO SAVEPOINT before_fetch_error;
CLOSE fetch_failure;
DROP TABLE cursor_fetch_failure;
COMMIT;

CREATE TABLE cursor_wide_input(id integer, payload text);
INSERT INTO cursor_wide_input VALUES(7,repeat('x',131072));
BEGIN;
DECLARE narrow_input NO SCROLL CURSOR FOR SELECT id FROM cursor_wide_input;
UPDATE cursor_wide_input SET id=9;
FETCH ALL FROM narrow_input;
CLOSE narrow_input;
COMMIT;
DROP TABLE cursor_wide_input;

CREATE TABLE cursor_forward_input(id integer);
INSERT INTO cursor_forward_input SELECT g FROM generate_series(1,8) AS g;
BEGIN;
DECLARE forward_input NO SCROLL CURSOR FOR SELECT id FROM cursor_forward_input;
-- A sequential scan begins on the first requested tuple, not at DECLARE.
-- https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/executor/nodeSeqscan.c
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_forward_input';
MOVE BACKWARD ALL FROM forward_input;
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_forward_input';
FETCH NEXT FROM forward_input;
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_forward_input';
FETCH ABSOLUTE 5 FROM forward_input;
FETCH RELATIVE 2 FROM forward_input;
MOVE FORWARD 1 FROM forward_input;
MOVE 0 FROM forward_input;
FETCH NEXT FROM forward_input;
FETCH 0 FROM forward_input;
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_forward_input';
CLOSE forward_input;
DECLARE negative_input NO SCROLL CURSOR FOR SELECT id FROM cursor_forward_input;
SAVEPOINT before_last;
FETCH LAST FROM negative_input;
ROLLBACK TO SAVEPOINT before_last;
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_forward_input';
FETCH NEXT FROM negative_input;
ROLLBACK TO SAVEPOINT before_last;
CLOSE negative_input;
COMMIT;
DROP TABLE cursor_forward_input;

-- Read counts describe work performed, including reads inside rolled-back savepoints.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/activity/pgstat_relation.c
CREATE TABLE cursor_savepoint_input(id integer);
INSERT INTO cursor_savepoint_input VALUES (1),(2),(3);
BEGIN;
SAVEPOINT outer_reads;
DECLARE savepoint_input NO SCROLL CURSOR FOR SELECT id FROM cursor_savepoint_input;
FETCH NEXT FROM savepoint_input;
SAVEPOINT inner_reads;
FETCH NEXT FROM savepoint_input;
RELEASE SAVEPOINT inner_reads;
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_savepoint_input';
ROLLBACK TO SAVEPOINT outer_reads;
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_savepoint_input';
DECLARE remaining_input NO SCROLL CURSOR FOR SELECT id FROM cursor_savepoint_input;
FETCH ALL FROM remaining_input;
RELEASE SAVEPOINT outer_reads;
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_savepoint_input';
ROLLBACK;
-- PostgreSQL may keep backend-local read counts until its next statistics flush.
SELECT pg_stat_force_next_flush();
BEGIN;
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_savepoint_input';
SAVEPOINT old_table;
CREATE TABLE cursor_reused_input(id integer);
INSERT INTO cursor_reused_input VALUES (1),(2);
DECLARE reused_input NO SCROLL CURSOR FOR SELECT id FROM cursor_reused_input;
FETCH ALL FROM reused_input;
ROLLBACK TO SAVEPOINT old_table;
CREATE TABLE cursor_reused_input(id integer);
SELECT seq_scan,seq_tup_read,n_tup_ins FROM pg_stat_xact_user_tables WHERE relname='cursor_reused_input';
RELEASE SAVEPOINT old_table;
SELECT seq_scan,seq_tup_read,n_tup_ins FROM pg_stat_xact_user_tables WHERE relname='cursor_reused_input';
COMMIT;
DROP TABLE cursor_reused_input;
DROP TABLE cursor_savepoint_input;

-- Per-row expressions and filters advance with FETCH. Constant/type errors
-- remain analysis/plan errors, even if the input table is empty.
CREATE TABLE cursor_expression_input(id integer);
INSERT INTO cursor_expression_input VALUES (1),(2),(0);
BEGIN;
DECLARE calculated NO SCROLL CURSOR FOR SELECT 100/id AS quotient FROM cursor_expression_input;
FETCH NEXT FROM calculated;
FETCH NEXT FROM calculated;
SAVEPOINT expression_error;
FETCH NEXT FROM calculated;
ROLLBACK TO SAVEPOINT expression_error;
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_expression_input';
FETCH NEXT FROM calculated;
ROLLBACK TO SAVEPOINT expression_error;
CLOSE calculated;
ROLLBACK;
BEGIN;
DECLARE filtered NO SCROLL CURSOR FOR
SELECT id*10 AS scaled FROM cursor_expression_input WHERE id<>0 AND NOT (id IS NULL);
FETCH NEXT FROM filtered;
FETCH ALL FROM filtered;
CLOSE filtered;
COMMIT;
BEGIN;
DECLARE constant_error NO SCROLL CURSOR FOR SELECT 1/0 FROM cursor_expression_input;
ROLLBACK;
TRUNCATE cursor_expression_input;
BEGIN;
DECLARE typed_error NO SCROLL CURSOR FOR SELECT id FROM cursor_expression_input WHERE id;
ROLLBACK;
BEGIN;
DECLARE literal_error NO SCROLL CURSOR FOR SELECT id FROM cursor_expression_input WHERE 'invalid';
ROLLBACK;
INSERT INTO cursor_expression_input VALUES (1);
BEGIN;
DECLARE literal_filter NO SCROLL CURSOR FOR SELECT id FROM cursor_expression_input WHERE 't';
FETCH ALL FROM literal_filter;
COMMIT;
DROP TABLE cursor_expression_input;

CREATE TABLE cursor_empty_input(id integer);
INSERT INTO cursor_empty_input VALUES (1);
BEGIN;
DECLARE empty_filter NO SCROLL CURSOR FOR SELECT id FROM cursor_empty_input WHERE FALSE;
FETCH ALL FROM empty_filter;
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_empty_input';
DROP TABLE cursor_empty_input;
FETCH ALL FROM empty_filter;
COMMIT;

CREATE TABLE cursor_bounded_input(id integer);
INSERT INTO cursor_bounded_input SELECT g FROM generate_series(1,8) AS g;
BEGIN;
DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM cursor_bounded_input LIMIT 1+1 OFFSET 2;
FETCH NEXT FROM bounded;
FETCH NEXT FROM bounded;
FETCH ALL FROM bounded;
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_bounded_input';
COMMIT;
SELECT pg_stat_force_next_flush();
BEGIN;
DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM cursor_bounded_input LIMIT 0 OFFSET 5;
FETCH ALL FROM bounded;
SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='cursor_bounded_input';
COMMIT;
BEGIN;
DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM cursor_bounded_input LIMIT NULL OFFSET NULL;
FETCH ALL FROM bounded;
COMMIT;
BEGIN;
DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM cursor_bounded_input LIMIT -1;
FETCH NEXT FROM bounded;
ROLLBACK;
BEGIN;
DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM cursor_bounded_input OFFSET -1;
FETCH NEXT FROM bounded;
ROLLBACK;
BEGIN;
DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM cursor_bounded_input WHERE FALSE LIMIT -1;
FETCH NEXT FROM bounded;
ROLLBACK;
BEGIN;
DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM cursor_bounded_input LIMIT 1/0;
ROLLBACK;
DROP TABLE cursor_bounded_input;

-- OFFSET consumes child results, so an error in a skipped projection still fails.
CREATE TABLE cursor_offset_error(id integer);
INSERT INTO cursor_offset_error VALUES (0),(1);
BEGIN;
DECLARE skipped_error NO SCROLL CURSOR FOR SELECT 100/id FROM cursor_offset_error OFFSET 1;
FETCH NEXT FROM skipped_error;
ROLLBACK;
DROP TABLE cursor_offset_error;

CREATE TABLE cursor_scroll_input(id integer);
INSERT INTO cursor_scroll_input SELECT g FROM generate_series(1,8) AS g;
BEGIN;
DECLARE scrolling SCROLL CURSOR FOR SELECT id*10 AS scaled FROM cursor_scroll_input WHERE id%2=0;
UPDATE cursor_scroll_input SET id=id+100;
FETCH PRIOR FROM scrolling;
FETCH NEXT FROM scrolling;
FETCH 0 FROM scrolling;
FETCH NEXT FROM scrolling;
FETCH PRIOR FROM scrolling;
MOVE FORWARD 2 FROM scrolling;
FETCH 0 FROM scrolling;
FETCH LAST FROM scrolling;
FETCH BACKWARD ALL FROM scrolling;
FETCH FIRST FROM scrolling;
FETCH ABSOLUTE -2 FROM scrolling;
FETCH RELATIVE -1 FROM scrolling;
CLOSE scrolling;
ROLLBACK;
DROP TABLE cursor_scroll_input;

CREATE TABLE cursor_hold_input(id integer);
INSERT INTO cursor_hold_input VALUES (1),(2),(3),(4);
BEGIN;
DECLARE held NO SCROLL CURSOR WITH HOLD FOR SELECT id*10 AS scaled FROM cursor_hold_input;
FETCH NEXT FROM held;
COMMIT;
DROP TABLE cursor_hold_input;
FETCH NEXT FROM held;
BEGIN;
FETCH NEXT FROM held;
ROLLBACK;
FETCH NEXT FROM held;
CLOSE held;
BEGIN;
CREATE TEMPORARY TABLE temporary_hold_input(id integer) ON COMMIT DROP;
INSERT INTO temporary_hold_input VALUES (7);
DECLARE held SCROLL CURSOR WITH HOLD FOR SELECT id FROM temporary_hold_input;
COMMIT;
FETCH ALL FROM held;
FETCH FIRST FROM held;
CLOSE held;

CREATE TABLE cursor_hold_error(id integer);
INSERT INTO cursor_hold_error VALUES (1),(0);
BEGIN;
DECLARE held NO SCROLL CURSOR WITH HOLD FOR SELECT 100/id AS quotient FROM cursor_hold_error;
FETCH NEXT FROM held;
COMMIT;
FETCH NEXT FROM held;
DROP TABLE cursor_hold_error;

CREATE TABLE automatic_hold_input(id integer);
INSERT INTO automatic_hold_input VALUES (1),(2);
DECLARE automatic_hold NO SCROLL CURSOR WITH HOLD FOR SELECT id FROM automatic_hold_input;
BEGIN;
FETCH NEXT FROM automatic_hold;
ROLLBACK;
FETCH NEXT FROM automatic_hold;
CLOSE automatic_hold;
DROP TABLE automatic_hold_input;

CREATE TABLE hold_trigger_input(id integer);
INSERT INTO hold_trigger_input VALUES (0);
CREATE TABLE hold_trigger_gate(id integer);
CREATE FUNCTION hold_trigger_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'deferred gate'; END $$;
CREATE CONSTRAINT TRIGGER hold_gate AFTER INSERT ON hold_trigger_gate DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION hold_trigger_failure();
BEGIN;
DECLARE held NO SCROLL CURSOR WITH HOLD FOR SELECT 100/id AS quotient FROM hold_trigger_input;
INSERT INTO hold_trigger_gate VALUES (1);
COMMIT;
FETCH NEXT FROM held;
SELECT count(*) FROM hold_trigger_gate;
DROP TABLE hold_trigger_input,hold_trigger_gate;
DROP FUNCTION hold_trigger_failure();

-- RETURN can be nested or absent; reaching the end is checked during execution.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/pl/plpgsql/src/pl_exec.c#L1005-L1011
CREATE TABLE trigger_return_input(id integer);
CREATE FUNCTION trigger_conditional_return() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN IF NEW.id>0 THEN RETURN NEW; END IF; END $$;
CREATE TRIGGER conditional_return BEFORE INSERT ON trigger_return_input
FOR EACH ROW EXECUTE FUNCTION trigger_conditional_return();
INSERT INTO trigger_return_input VALUES (1);
INSERT INTO trigger_return_input VALUES (0);
DROP TRIGGER conditional_return ON trigger_return_input;
CREATE FUNCTION trigger_missing_statement_return() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NULL; END $$;
CREATE TRIGGER missing_statement_return AFTER INSERT ON trigger_return_input
FOR EACH STATEMENT EXECUTE FUNCTION trigger_missing_statement_return();
INSERT INTO trigger_return_input VALUES (2);
SELECT id FROM trigger_return_input;
DROP TABLE trigger_return_input;
DROP FUNCTION trigger_conditional_return();
DROP FUNCTION trigger_missing_statement_return();

CREATE TABLE default_cursor_input(id integer);
INSERT INTO default_cursor_input VALUES (1),(2),(3);
BEGIN;
DECLARE automatic CURSOR FOR SELECT id FROM default_cursor_input;
SELECT is_scrollable FROM pg_cursors WHERE name='automatic';
FETCH NEXT FROM automatic;
FETCH LAST FROM automatic;
FETCH FIRST FROM automatic;
CLOSE automatic;
COMMIT;
BEGIN;
DECLARE empty_cursor CURSOR FOR SELECT id FROM default_cursor_input WHERE 'f';
SELECT is_scrollable FROM pg_cursors WHERE name='empty_cursor';
FETCH ALL FROM empty_cursor;
CLOSE empty_cursor;
COMMIT;
BEGIN;
DECLARE empty_cursor CURSOR FOR SELECT id FROM default_cursor_input WHERE NULL AND TRUE;
SELECT is_scrollable FROM pg_cursors WHERE name='empty_cursor';
FETCH ALL FROM empty_cursor;
CLOSE empty_cursor;
COMMIT;
CREATE MATERIALIZED VIEW default_empty_view AS SELECT id FROM default_cursor_input WITH NO DATA;
BEGIN;
DECLARE empty_cursor CURSOR FOR SELECT id FROM default_empty_view WHERE 'f';
FETCH ALL FROM empty_cursor;
COMMIT;
DROP MATERIALIZED VIEW default_empty_view;
CREATE ROLE cursor_denied;
SET ROLE cursor_denied;
BEGIN;
DECLARE bad NO SCROLL CURSOR FOR SELECT id+TRUE FROM default_cursor_input;
ROLLBACK;
BEGIN;
DECLARE bad NO SCROLL CURSOR FOR SELECT id FROM default_cursor_input;
ROLLBACK;
RESET ROLE;
DROP ROLE cursor_denied;
CREATE FUNCTION default_parameter_cursor(keep boolean) RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'parameter_default'; BEGIN
  OPEN c FOR SELECT id FROM default_cursor_input WHERE keep; RETURN c;
END $$;
BEGIN;
SELECT default_parameter_cursor(false);
SELECT is_scrollable FROM pg_cursors WHERE name='parameter_default';
FETCH ALL FROM parameter_default;
COMMIT;
DROP FUNCTION default_parameter_cursor(boolean);
DROP TABLE default_cursor_input;

-- Name resolution at DECLARE survives a new operator on the same search path.
CREATE SCHEMA cursor_ops;
SET search_path=cursor_ops,pg_catalog;
CREATE TABLE operator_input(id integer);
INSERT INTO operator_input VALUES (1),(2);
BEGIN;
DECLARE bound_ops NO SCROLL CURSOR FOR SELECT id+10 AS result FROM operator_input;
CREATE FUNCTION cursor_ops.custom_add(integer,integer) RETURNS integer LANGUAGE SQL RETURN $1-$2;
CREATE OPERATOR cursor_ops.+ (FUNCTION=cursor_ops.custom_add,LEFTARG=integer,RIGHTARG=integer);
FETCH NEXT FROM bound_ops;
FETCH NEXT FROM bound_ops;
SELECT id+10 AS result FROM operator_input;
CLOSE bound_ops;
COMMIT;
RESET search_path;
DROP SCHEMA cursor_ops CASCADE;

-- A record wider than one byte must not lose fields or the following column.
BEGIN;
DECLARE wide_record SCROLL CURSOR FOR SELECT ROW(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 235, 236, 237, 238, 239, 240, 241, 242, 243, 244, 245, 246, 247, 248, 249, 250, 251, 252, 253, 254, 255, 256) AS value, 99 AS following;
FETCH NEXT FROM wide_record;
COMMIT;

-- https://www.postgresql.org/docs/18/typeconv-union-case.html
CREATE DOMAIN cursor_day AS date;
SELECT pg_typeof(day) FROM (SELECT NULL::cursor_day AS day UNION ALL SELECT NULL::cursor_day) AS same_domain LIMIT 1;
SELECT pg_typeof(day) FROM (SELECT NULL::cursor_day AS day UNION ALL SELECT NULL) AS mixed_domain LIMIT 1;
CREATE FUNCTION cursor_set_locals(p integer) RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'set_locals'; label text := 'opening value'; d cursor_day;
BEGIN
  OPEN c FOR
    SELECT label AS label, d AS day, p AS input
    UNION ALL SELECT label, d, p + 1
    ORDER BY input;
  label := 'changed';
  p := 900;
  RETURN c;
END $$;
BEGIN;
SELECT cursor_set_locals(17);
FETCH ALL FROM set_locals;
CLOSE set_locals;
COMMIT;
DROP FUNCTION cursor_set_locals(integer);
SELECT pg_typeof(f2), pg_typeof(day) FROM (
  SELECT (ROW(1, 2)).*, NULL::cursor_day AS day
  UNION ALL SELECT (ROW(3, 4)).*, NULL::cursor_day
) AS expanded_record LIMIT 1;
CREATE TABLE cursor_domain_source(day cursor_day);
INSERT INTO cursor_domain_source VALUES (NULL);
SELECT pg_typeof(day) FROM (
  SELECT * FROM cursor_domain_source UNION ALL SELECT * FROM cursor_domain_source
) AS expanded_table LIMIT 1;
DROP TABLE cursor_domain_source;
DROP DOMAIN cursor_day;

CREATE TABLE cursor_local_source(id integer);
INSERT INTO cursor_local_source VALUES (1), (2), (3);
CREATE FUNCTION cursor_table_local() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'table_local'; label text := 'opening value'; cutoff integer := 2;
BEGIN
  OPEN c FOR SELECT id, label, label::text FROM cursor_local_source WHERE id <= cutoff ORDER BY id;
  label := 'changed';
  cutoff := 0;
  RETURN c;
END $$;
BEGIN;
SELECT cursor_table_local();
FETCH ALL FROM table_local;
CLOSE table_local;
COMMIT;
DROP FUNCTION cursor_table_local();
DROP TABLE cursor_local_source;

CREATE TABLE cursor_ambiguous_source(id integer);
INSERT INTO cursor_ambiguous_source VALUES (1);
CREATE SEQUENCE cursor_ambiguous_calls;
CREATE FUNCTION cursor_ambiguous_local() RETURNS void LANGUAGE plpgsql AS $$
DECLARE c refcursor; id integer := 10;
BEGIN
  OPEN c FOR SELECT nextval('cursor_ambiguous_calls'), id FROM cursor_ambiguous_source;
  CLOSE c;
END $$;
SELECT cursor_ambiguous_local();
SELECT last_value, is_called FROM cursor_ambiguous_calls;
DROP FUNCTION cursor_ambiguous_local();
DROP TABLE cursor_ambiguous_source;
DROP SEQUENCE cursor_ambiguous_calls;

CREATE FUNCTION cursor_record_local() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'record_local'; r record;
BEGIN
  SELECT 'opening value'::text AS label INTO r;
  OPEN c FOR SELECT r.label AS label UNION ALL SELECT r.label;
  SELECT 'changed'::text AS label INTO r;
  RETURN c;
END $$;
BEGIN;
SELECT cursor_record_local();
FETCH ALL FROM record_local;
CLOSE record_local;
COMMIT;
DROP FUNCTION cursor_record_local();

CREATE FUNCTION cursor_wide_record_field() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'wide_record_field'; r record;
BEGIN
  SELECT ROW(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 235, 236, 237, 238, 239, 240, 241, 242, 243, 244, 245, 246, 247, 248, 249, 250, 251, 252, 253, 254, 255, 256) AS payload INTO r;
  OPEN c FOR SELECT ((r.payload).f256)::integer;
  RETURN c;
END $$;
BEGIN;
SELECT cursor_wide_record_field();
FETCH ALL FROM wide_record_field;
CLOSE wide_record_field;
COMMIT;
DROP FUNCTION cursor_wide_record_field();
SELECT (ROW(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65)).*;

-- USING values carry the expression's type, including NULL, independently of
-- the enclosing routine's arguments. Dynamic SQL cannot see its local names.
-- Utility EXECUTE arguments do not accept SPI parameters (PostgreSQL 18.6).
CREATE FUNCTION cursor_dynamic_null(p integer) RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'dynamic_null'; d date;
BEGIN
  OPEN c FOR EXECUTE 'SELECT pg_typeof($1)::text AS kind, $1 AS day' USING d;
  RETURN c;
END $$;
BEGIN;
SELECT cursor_dynamic_null(17);
FETCH ALL FROM dynamic_null;
CLOSE dynamic_null;
COMMIT;
DROP FUNCTION cursor_dynamic_null(integer);
CREATE FUNCTION cursor_dynamic_record(p integer) RETURNS text LANGUAGE plpgsql AS $$
DECLARE d date; r record;
BEGIN
  EXECUTE 'SELECT pg_typeof($1)::text AS kind, $1 AS day' INTO r USING d;
  RETURN r.kind || ':' || pg_typeof(r.day)::text;
END $$;
SELECT cursor_dynamic_record(17);
DROP FUNCTION cursor_dynamic_record(integer);
CREATE FUNCTION cursor_dynamic_literals() RETURNS text LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
  EXECUTE 'SELECT pg_typeof($1)::text AS kind' INTO r USING NULL;
  RETURN r.kind;
END $$;
SELECT cursor_dynamic_literals();
DROP FUNCTION cursor_dynamic_literals();
CREATE FUNCTION cursor_dynamic_scope() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE hidden integer := 42; result integer;
BEGIN
  EXECUTE 'SELECT hidden' INTO result;
  RETURN result;
END $$;
SELECT cursor_dynamic_scope();
DROP FUNCTION cursor_dynamic_scope();
PREPARE cursor_dynamic_prepared(date) AS SELECT pg_typeof($1)::text AS kind, $1 AS day;
CREATE FUNCTION cursor_dynamic_execute(p integer) RETURNS text LANGUAGE plpgsql AS $$
DECLARE d date; r record;
BEGIN
  EXECUTE 'EXECUTE cursor_dynamic_prepared($1)' INTO r USING d;
  RETURN r.kind || ':' || pg_typeof(r.day)::text;
END $$;
SELECT cursor_dynamic_execute(17);
DROP FUNCTION cursor_dynamic_execute(integer);
DEALLOCATE cursor_dynamic_prepared;

CREATE DOMAIN cursor_argument_day AS date;
CREATE FUNCTION cursor_domain_sql(cursor_argument_day) RETURNS text LANGUAGE sql AS $$
  SELECT pg_typeof($1)::text
$$;
SELECT cursor_domain_sql(NULL::cursor_argument_day);
DROP FUNCTION cursor_domain_sql(cursor_argument_day);
CREATE FUNCTION cursor_domain_argument(cursor_argument_day) RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'domain_argument';
BEGIN
  OPEN c FOR SELECT pg_typeof($1)::text AS kind, $1 AS day;
  RETURN c;
END $$;
BEGIN;
SELECT cursor_domain_argument(NULL::cursor_argument_day);
FETCH ALL FROM domain_argument;
CLOSE domain_argument;
COMMIT;
DROP FUNCTION cursor_domain_argument(cursor_argument_day);
CREATE FUNCTION cursor_dynamic_domain(p cursor_argument_day) RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'dynamic_domain';
BEGIN
  OPEN c FOR EXECUTE 'SELECT pg_typeof($1)::text AS kind, $1 AS day' USING p;
  RETURN c;
END $$;
BEGIN;
SELECT cursor_dynamic_domain(NULL::cursor_argument_day);
FETCH ALL FROM dynamic_domain;
CLOSE dynamic_domain;
COMMIT;
DROP FUNCTION cursor_dynamic_domain(cursor_argument_day);
SELECT pg_typeof(f1), following FROM (
  SELECT (ROW(NULL::cursor_argument_day)).*, 99 AS following
  UNION ALL SELECT (ROW(NULL::cursor_argument_day)).*, 100
) AS domain_record LIMIT 1;
SELECT (ROW(NULL::cursor_argument_day)).*;
SELECT (ROW(1)).missing::cursor_argument_day;
DROP DOMAIN cursor_argument_day;

SELECT (ROW(1)).missing::integer;

-- Expansion must enforce the complete ROW width before evaluation.
-- https://doxygen.postgresql.org/parse__expr_8c_source.html
SELECT ROW((ROW(1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1)).*, 1);
