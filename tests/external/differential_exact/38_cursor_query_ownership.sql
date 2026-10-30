-- BETWEEN expands to ordinary comparisons, including repeated operands.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/parser/parse_expr.c#L1252
SELECT 1 BETWEEN 2 AND NULL,1 NOT BETWEEN 2 AND NULL,
       1 BETWEEN NULL AND 0,1 NOT BETWEEN NULL AND 0,
       1 BETWEEN NULL AND 2,1 NOT BETWEEN NULL AND 2;
SELECT 1 BETWEEN SYMMETRIC 2 AND 0,1 NOT BETWEEN SYMMETRIC 2 AND 0,
       1 BETWEEN SYMMETRIC NULL AND 0,1 NOT BETWEEN SYMMETRIC NULL AND NULL;
CREATE SEQUENCE between_calls START 10;
SELECT nextval('between_calls') BETWEEN 10 AND 11;
SELECT last_value FROM between_calls;
ALTER SEQUENCE between_calls RESTART WITH 10;
SELECT nextval('between_calls') NOT BETWEEN 100 AND nextval('between_calls');
SELECT last_value FROM between_calls;
ALTER SEQUENCE between_calls RESTART WITH 10;
SELECT (SELECT nextval('between_calls')) BETWEEN 10 AND 11;
SELECT last_value FROM between_calls;
ALTER SEQUENCE between_calls RESTART WITH 10;
SELECT (1+(SELECT nextval('between_calls'))) BETWEEN 11 AND 12;
SELECT last_value FROM between_calls;
ALTER SEQUENCE between_calls RESTART WITH 10;
SELECT (SELECT (SELECT nextval('between_calls'))) BETWEEN 10 AND 11;
SELECT last_value FROM between_calls;
ALTER SEQUENCE between_calls RESTART WITH 10;
SELECT nextval('between_calls') BETWEEN SYMMETRIC 1 AND 2;
SELECT last_value FROM between_calls;
ALTER SEQUENCE between_calls RESTART WITH 10;
SELECT nextval('between_calls') NOT BETWEEN SYMMETRIC 100 AND 0;
SELECT last_value FROM between_calls;
DROP SEQUENCE between_calls;

CREATE TABLE between_rows(id integer,lo integer,hi integer);
INSERT INTO between_rows VALUES(1,0,2),(3,0,4),(9,5,6);
BEGIN;
DECLARE between_later NO SCROLL CURSOR FOR
SELECT id,id BETWEEN lo AND 100/(id-9) FROM between_rows;
FETCH 2 FROM between_later;
SAVEPOINT before_between_error;
FETCH NEXT FROM between_later;
ROLLBACK TO SAVEPOINT before_between_error;
FETCH NEXT FROM between_later;
ROLLBACK;
BEGIN;
DECLARE between_filtered NO SCROLL CURSOR FOR
SELECT id FROM between_rows WHERE id BETWEEN 100 AND 1/(id-9);
FETCH ALL FROM between_filtered;
COMMIT;
CREATE SCHEMA between_scope;
SET search_path=between_scope,pg_catalog;
BEGIN;
DECLARE between_bound NO SCROLL CURSOR FOR
SELECT id,id BETWEEN 0 AND 4 FROM public.between_rows;
CREATE FUNCTION between_scope.ge(integer,integer) RETURNS boolean
LANGUAGE sql IMMUTABLE AS 'SELECT false';
CREATE OPERATOR between_scope.>=
(FUNCTION=between_scope.ge,LEFTARG=integer,RIGHTARG=integer);
SELECT 1>=0;
FETCH ALL FROM between_bound;
COMMIT;
SET search_path=public;
DROP OPERATOR between_scope.>=(integer,integer);
DROP FUNCTION between_scope.ge(integer,integer);
DROP SCHEMA between_scope;
CREATE VIEW between_view AS SELECT id BETWEEN lo AND hi AS inside FROM between_rows;
ALTER TABLE between_rows RENAME COLUMN id TO renamed;
SELECT inside FROM between_view;
DROP VIEW between_view;
DROP TABLE between_rows;

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
SELECT pg_typeof(DATE '2026-10-03' OPERATOR(pg_catalog.-) DATE '2026-10-01'),
       (DATE '2026-10-03' OPERATOR(pg_catalog.-) DATE '2026-10-01') + 1;
CREATE TABLE conditional_input(id integer, day date, start_day date, at timestamp, start_at timestamp);
INSERT INTO conditional_input VALUES
(1,DATE '2026-10-03',DATE '2026-10-01',TIMESTAMP '2026-10-03',TIMESTAMP '2026-10-01'),
(2,DATE '2026-10-06',DATE '2026-10-01',TIMESTAMP '2026-10-06',TIMESTAMP '2026-10-01');
BEGIN;
DECLARE formulas NO SCROLL CURSOR FOR
SELECT (day-start_day)+id AS elapsed,
       CASE WHEN id%2=0 THEN id*10 ELSE 2147483648 END AS chosen,
       CASE WHEN id=1 THEN at-start_at ELSE NULL END AS span
FROM conditional_input WHERE CASE WHEN id>0 THEN TRUE ELSE FALSE END;
FETCH NEXT FROM formulas;
FETCH NEXT FROM formulas;
CLOSE formulas;
DECLARE conditional_failure NO SCROLL CURSOR FOR
SELECT CASE WHEN id=2 THEN 100/(id-2) ELSE id END FROM conditional_input;
FETCH NEXT FROM conditional_failure;
SAVEPOINT before_conditional_failure;
FETCH NEXT FROM conditional_failure;
ROLLBACK TO SAVEPOINT before_conditional_failure;
CLOSE conditional_failure;
COMMIT;
DROP TABLE conditional_input;
CREATE TABLE array_input(id integer, vals integer[]);
INSERT INTO array_input VALUES(1,ARRAY[10,20,30]),(2,ARRAY[40,50,60]),(3,NULL);
BEGIN;
DECLARE arrays NO SCROLL CURSOR FOR
SELECT vals[id],(ARRAY[id,id+1])[2],vals[2:3] FROM array_input WHERE (ARRAY[id,0])[1]>0;
UPDATE array_input SET vals=ARRAY[99,99,99] WHERE id=2;
FETCH NEXT FROM arrays;
FETCH NEXT FROM arrays;
FETCH NEXT FROM arrays;
CLOSE arrays;
DECLARE index_failure NO SCROLL CURSOR FOR SELECT vals[1/(id-2)] FROM array_input WHERE id<=2;
FETCH NEXT FROM index_failure;
SAVEPOINT before_index_failure;
FETCH NEXT FROM index_failure;
ROLLBACK TO SAVEPOINT before_index_failure;
CLOSE index_failure;
ROLLBACK;
CREATE FUNCTION captured_array(input integer[]) RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'saved_array';
BEGIN
  OPEN c NO SCROLL FOR SELECT input[id] FROM array_input;
  input := ARRAY[99,99,99];
  RETURN c;
END $$;
BEGIN;
SELECT captured_array(ARRAY[7,8,9]);
FETCH ALL FROM saved_array;
CLOSE saved_array;
COMMIT;
DROP FUNCTION captured_array(integer[]);
-- PostgreSQL resolves all constructor declarations before executing members.
-- https://www.postgresql.org/docs/18/typeconv-union-case.html
SELECT pg_typeof(ARRAY[1::smallint,2]),pg_typeof(ARRAY[1,2147483648]),
       pg_typeof(ARRAY[1::real,2::numeric]),
       pg_typeof(ARRAY[DATE '2026-10-01',TIMESTAMP '2026-10-02']);
SELECT ARRAY[],ARRAY[1];
SELECT ARRAY[]::integer[],ARRAY[ARRAY[]::integer[],NULL::integer[]];
SELECT ARRAY[1,TRUE];
SELECT ARRAY[1::money,2];
SELECT ARRAY[NULL::"char",NULL::pg_node_tree,1];
SELECT CASE WHEN true THEN NULL::pg_node_tree WHEN false THEN 1 ELSE NULL::"char" END;
SELECT ARRAY[ARRAY[1],NULL::integer[]];
SELECT ARRAY[NULL::integer[],NULL::integer[]];
SELECT ARRAY[ARRAY[1::smallint],ARRAY[2147483648]],ARRAY[NULL,DATE '2026-10-01'];
SELECT ARRAY[ARRAY[1],ARRAY[TRUE]];
SELECT ARRAY[ARRAY[NULL],ARRAY[1]];
SELECT pg_typeof(ARRAY[1]||ARRAY[2147483648]),pg_typeof(ARRAY[1::real]||ARRAY[2::numeric]),
       ARRAY[1]||ARRAY[2147483648],ARRAY[1::smallint]||2,3||ARRAY[4::smallint];
SELECT ARRAY[1]||ARRAY[TRUE];
SELECT typname,typcategory FROM pg_type WHERE typname IN ('timetz','char','bit','varbit','json','jsonb') ORDER BY typname;
BEGIN;
DECLARE typed_arrays NO SCROLL CURSOR FOR SELECT
CASE WHEN id=1 THEN ARRAY[id,2147483648] ELSE ARRAY[id,id+1] END,
CASE WHEN id=1 THEN vals[1:2] ELSE ARRAY[2147483648] END,
ARRAY[ARRAY[id],ARRAY[2147483648]] FROM array_input;
FETCH NEXT FROM typed_arrays;
FETCH NEXT FROM typed_arrays;
FETCH ALL FROM typed_arrays;
CLOSE typed_arrays;
COMMIT;
CREATE DOMAIN array_day AS date;
CREATE FUNCTION captured_null_array(input array_day) RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'saved_null_array';
BEGIN
  OPEN c NO SCROLL FOR SELECT ARRAY[input] FROM array_input;
  input := DATE '2026-10-30';
  RETURN c;
END $$;
BEGIN;
SELECT captured_null_array(NULL);
FETCH NEXT FROM saved_null_array;
FETCH ALL FROM saved_null_array;
CLOSE saved_null_array;
COMMIT;
DROP FUNCTION captured_null_array(array_day);
SELECT pg_typeof(ARRAY[NULL::array_day,NULL::array_day]),pg_typeof(ARRAY[NULL::array_day,NULL]),pg_typeof(ARRAY[NULL::array_day,DATE '2026-10-01']);
DROP DOMAIN array_day;
CREATE SEQUENCE invalid_array_effect;
SELECT ARRAY[TRUE,nextval('invalid_array_effect')];
SELECT last_value,is_called FROM invalid_array_effect;
DROP SEQUENCE invalid_array_effect;
SELECT pg_typeof(ARRAY[(SELECT 1)]),pg_typeof(ARRAY[(SELECT NULL::date)]),
       pg_typeof(ARRAY[(SELECT ARRAY[1,2])]),ARRAY[(SELECT 1),(SELECT 2147483648)];
CREATE DOMAIN subquery_day AS date;
CREATE TABLE subquery_arrays(id integer,d subquery_day);
INSERT INTO subquery_arrays VALUES(1,NULL),(2,DATE '2026-10-30');
SELECT id,pg_typeof(ARRAY[(SELECT d)]),ARRAY[(SELECT d)] FROM subquery_arrays ORDER BY id;
SELECT id,pg_typeof(ARRAY[(SELECT (SELECT d))]) FROM subquery_arrays ORDER BY id;
CREATE SEQUENCE subquery_array_calls;
CREATE FUNCTION native_array_choice(choose boolean) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  RETURN pg_typeof(CASE WHEN choose THEN ARRAY[7] ELSE ARRAY[(SELECT nextval('subquery_array_calls'))] END)::text;
END $$;
CREATE FUNCTION native_domain_array(d subquery_day) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  RETURN pg_typeof(ARRAY[(SELECT d)])::text;
END $$;
SELECT native_array_choice(true),native_domain_array(NULL);
SELECT last_value,is_called FROM subquery_array_calls;
SELECT native_array_choice(false);
SELECT last_value,is_called FROM subquery_array_calls;
CREATE FUNCTION next_array_call() RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT nextval('subquery_array_calls') $$;
SELECT pg_typeof(next_array_call());
SELECT last_value FROM subquery_array_calls;
SELECT pg_typeof(ARRAY[(SELECT id FROM subquery_arrays)]);
DROP FUNCTION next_array_call(),native_array_choice(boolean),native_domain_array(subquery_day);
DROP SEQUENCE subquery_array_calls;
DROP TABLE subquery_arrays;
DROP DOMAIN subquery_day;
CREATE TYPE array_mood AS ENUM('ready','done');
CREATE DOMAIN array_positive AS integer CHECK(VALUE>0);
CREATE DOMAIN array_required AS integer NOT NULL;
SELECT ARRAY['ready',NULL,'done']::array_mood[],ARRAY['1','2']::array_positive[],
       ARRAY['pg_class']::regclass[],ARRAY['integer']::regtype[];
SELECT ARRAY['x'::character(3)]::text[],ARRAY['x'::character(3)];
SELECT ARRAY['bad']::array_mood[];
SELECT ARRAY[-1]::array_positive[];
SELECT ARRAY[NULL]::array_required[];
SELECT pg_typeof(log(2,8)),pg_typeof(date_trunc('year',DATE '2020-02-03'));
SELECT pg_typeof(bit_and(v)) FROM (VALUES(B'11'::varbit),(NULL::varbit)) AS t(v);
SELECT log(NULL::real,1::real);
DROP TYPE array_mood;
DROP DOMAIN array_positive,array_required;

CREATE SEQUENCE effect_calls;
CREATE TABLE effect_rows(id integer);
INSERT INTO effect_rows VALUES(1),(2),(3);
SELECT (SELECT nextval('effect_calls'));
SELECT id,(SELECT nextval('effect_calls')) FROM effect_rows ORDER BY id;
SELECT id,(SELECT nextval('effect_calls')+id) FROM effect_rows ORDER BY id;
SELECT EXISTS(SELECT (SELECT nextval('effect_calls')) FROM effect_rows);
SELECT last_value FROM effect_calls;
SELECT sum(id)+(SELECT nextval('effect_calls')) FROM effect_rows;
SELECT id,sum(id) OVER()+(SELECT nextval('effect_calls')) FROM effect_rows ORDER BY id;
SELECT ARRAY(SELECT nextval('effect_calls') FROM effect_rows ORDER BY id);
SELECT (SELECT nextval('effect_calls') UNION ALL SELECT 999 LIMIT 1);
UPDATE effect_rows SET id=id WHERE id=1 RETURNING (SELECT nextval('effect_calls')+id);
INSERT INTO effect_rows VALUES(4) RETURNING (SELECT nextval('effect_calls')+id);
DELETE FROM effect_rows WHERE id=4 RETURNING (SELECT nextval('effect_calls')+id);
MERGE INTO effect_rows AS t USING (VALUES(1)) AS x(id) ON t.id=x.id
WHEN MATCHED THEN UPDATE SET id=t.id RETURNING (SELECT nextval('effect_calls')+t.id);
CREATE SEQUENCE bound_calls;
-- PostgreSQL evaluates OFFSET before LIMIT.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeLimit.c#L339
SELECT id FROM effect_rows ORDER BY id OFFSET nextval('bound_calls') LIMIT nextval('bound_calls');
SELECT last_value FROM bound_calls;
SELECT id FROM effect_rows UNION ALL SELECT 4 ORDER BY 1 OFFSET nextval('bound_calls') LIMIT nextval('bound_calls');
SELECT last_value FROM bound_calls;
SELECT 1 LIMIT -1 OFFSET -1;
DROP TABLE effect_rows;
DROP SEQUENCE effect_calls,bound_calls;

CREATE DOMAIN cursor_short AS smallint;
CREATE DOMAIN cursor_network AS cidr;
CREATE DOMAIN cursor_day AS date;
CREATE DOMAIN cursor_vector AS integer[];
CREATE TABLE cursor_function_inputs(id integer, options text[], days cursor_day[]);
INSERT INTO cursor_function_inputs VALUES
(1,ARRAY['a','b'],ARRAY[DATE '2026-10-30',NULL]::cursor_day[]),
(2,NULL,NULL),(3,ARRAY['c'],ARRAY[]::cursor_day[]);
SELECT pg_typeof(mod(7::smallint,3::smallint)),pg_typeof(mod(NULL::smallint,3::smallint)),
       pg_typeof(mod(7::cursor_short,3::cursor_short)),
       pg_typeof(set_masklen('192.168.1.0/24'::cidr,16)),
       pg_typeof(set_masklen(NULL::cursor_network,16)),pg_typeof(set_masklen(NULL::inet,16));
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/int8.c
SELECT mod('-9223372036854775808'::bigint,-1::bigint),
       mod('-2147483648'::integer,-1),mod('-32768'::smallint,-1::smallint);
SELECT (SELECT 1 AS first_name UNION ALL SELECT 2 LIMIT 1),
       (SELECT lower('A') INTERSECT SELECT 'a');
SELECT pg_typeof(ARRAY(SELECT 1::smallint)),pg_typeof(ARRAY(SELECT NULL::numeric)),
       ARRAY(SELECT 1::smallint);
SELECT pg_typeof(ARRAY(SELECT ARRAY[1,2]::cursor_vector)),
       array_ndims(ARRAY(SELECT ARRAY[1,2]::cursor_vector)),
       cardinality(ARRAY(SELECT ARRAY[1,2]::cursor_vector));
SELECT id,ARRAY(SELECT 'x.'||option FROM unnest(t.options) option),
       pg_typeof(ARRAY(SELECT day_value FROM unnest(t.days) day_value)),
       ARRAY(SELECT day_value FROM unnest(t.days) day_value)
FROM cursor_function_inputs t ORDER BY id;
DROP TABLE cursor_function_inputs;
DROP DOMAIN cursor_short,cursor_network,cursor_day,cursor_vector;


BEGIN;
DECLARE wide_array NO SCROLL CURSOR FOR SELECT (ARRAY[id+0,id+1,id+2,id+3,id+4,id+5,id+6,id+7,id+8,id+9,id+10,id+11,id+12,id+13,id+14,id+15,id+16,id+17,id+18,id+19,id+20,id+21,id+22,id+23,id+24,id+25,id+26,id+27,id+28,id+29,id+30,id+31,id+32,id+33,id+34,id+35,id+36,id+37,id+38,id+39,id+40,id+41,id+42,id+43,id+44,id+45,id+46,id+47,id+48,id+49,id+50,id+51,id+52,id+53,id+54,id+55,id+56,id+57,id+58,id+59,id+60,id+61,id+62,id+63,id+64,id+65,id+66,id+67,id+68,id+69,id+70,id+71,id+72,id+73,id+74,id+75,id+76,id+77,id+78,id+79,id+80,id+81,id+82,id+83,id+84,id+85,id+86,id+87,id+88,id+89,id+90,id+91,id+92,id+93,id+94,id+95,id+96,id+97,id+98,id+99,id+100,id+101,id+102,id+103,id+104,id+105,id+106,id+107,id+108,id+109,id+110,id+111,id+112,id+113,id+114,id+115,id+116,id+117,id+118,id+119,id+120,id+121,id+122,id+123,id+124,id+125,id+126,id+127,id+128,id+129,id+130,id+131,id+132,id+133,id+134,id+135,id+136,id+137,id+138,id+139,id+140,id+141,id+142,id+143,id+144,id+145,id+146,id+147,id+148,id+149,id+150,id+151,id+152,id+153,id+154,id+155,id+156,id+157,id+158,id+159,id+160,id+161,id+162,id+163,id+164,id+165,id+166,id+167,id+168,id+169,id+170,id+171,id+172,id+173,id+174,id+175,id+176,id+177,id+178,id+179,id+180,id+181,id+182,id+183,id+184,id+185,id+186,id+187,id+188,id+189,id+190,id+191,id+192,id+193,id+194,id+195,id+196,id+197,id+198,id+199,id+200,id+201,id+202,id+203,id+204,id+205,id+206,id+207,id+208,id+209,id+210,id+211,id+212,id+213,id+214,id+215,id+216,id+217,id+218,id+219,id+220,id+221,id+222,id+223,id+224,id+225,id+226,id+227,id+228,id+229,id+230,id+231,id+232,id+233,id+234,id+235,id+236,id+237,id+238,id+239,id+240,id+241,id+242,id+243,id+244,id+245,id+246,id+247,id+248,id+249,id+250,id+251,id+252,id+253,id+254,id+255,id+256,id+257,id+258,id+259,id+260,id+261,id+262,id+263,id+264,id+265,id+266,id+267,id+268,id+269,id+270,id+271,id+272,id+273,id+274,id+275,id+276,id+277,id+278,id+279,id+280,id+281,id+282,id+283,id+284,id+285,id+286,id+287,id+288,id+289,id+290,id+291,id+292,id+293,id+294,id+295,id+296,id+297,id+298,id+299])[300] FROM array_input;
FETCH ALL FROM wide_array;
CLOSE wide_array;
COMMIT;
DROP TABLE array_input;
CREATE TABLE expression_input(id integer);
INSERT INTO expression_input VALUES (1),(2);
CREATE FUNCTION expression_value(wanted integer) RETURNS integer LANGUAGE plpgsql AS $$
DECLARE saved integer := (SELECT id FROM expression_input WHERE id=wanted);
BEGIN
  IF EXISTS (SELECT id FROM expression_input WHERE id=saved) THEN
    saved := (SELECT id+10 FROM expression_input WHERE id=saved);
  END IF;
  RETURN (SELECT saved);
END $$;
SELECT expression_value(1),expression_value(2),expression_value(3);
CREATE FUNCTION expression_many() RETURNS integer LANGUAGE plpgsql AS $$
BEGIN RETURN (SELECT id FROM expression_input); END $$;
SELECT expression_many();
CREATE FUNCTION expression_lazy(choose boolean) RETURNS integer LANGUAGE plpgsql AS $$
BEGIN
  IF choose OR EXISTS (SELECT id FROM expression_input WHERE 10/(id-id)>0) THEN RETURN 7; END IF;
  RETURN 0;
END $$;
CREATE FUNCTION expression_choice(choose boolean) RETURNS integer LANGUAGE plpgsql AS $$
BEGIN RETURN CASE WHEN choose THEN 7 ELSE (SELECT 10/(id-id) FROM expression_input WHERE id=1) END; END $$;
SELECT expression_lazy(true),expression_choice(true);
SELECT expression_lazy(false);
SELECT expression_choice(false);
CREATE FUNCTION expression_membership(wanted integer) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN RETURN (SELECT wanted) IN (SELECT id FROM expression_input); END $$;
CREATE FUNCTION expression_quantified(wanted integer) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN RETURN wanted > ALL (SELECT id FROM expression_input); END $$;
SELECT expression_membership(1),expression_membership(3),expression_membership(NULL),expression_quantified(3),expression_quantified(1);
CREATE TABLE expression_target(id integer);
CREATE FUNCTION expression_transition() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF EXISTS (SELECT id FROM expression_input WHERE id=NEW.id) THEN NEW.id := NEW.id+10; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER expression_transition BEFORE INSERT ON expression_target
FOR EACH ROW EXECUTE FUNCTION expression_transition();
INSERT INTO expression_target VALUES (1),(3) RETURNING id;
DROP TABLE expression_target;
DROP FUNCTION expression_transition();
DROP FUNCTION expression_many();
DROP FUNCTION expression_lazy(boolean);
DROP FUNCTION expression_choice(boolean);
DROP FUNCTION expression_membership(integer);
DROP FUNCTION expression_quantified(integer);
DROP FUNCTION expression_value(integer);
DROP TABLE expression_input;
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
-- The cursor owns the old 128 KiB row. Keep its replacement small enough for
-- both versions and their headers to fit the 256 KiB forced-spill reservation.
CHECKPOINT;
BEGIN;
DECLARE narrow_input NO SCROLL CURSOR FOR SELECT id FROM cursor_wide_input;
UPDATE cursor_wide_input SET id=9,payload='replacement';
FETCH ALL FROM narrow_input;
CLOSE narrow_input;
COMMIT;
SELECT id,length(payload) FROM cursor_wide_input;
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
BEGIN;
SELECT default_parameter_cursor(true);
SELECT is_scrollable FROM pg_cursors WHERE name='parameter_default';
FETCH LAST FROM parameter_default;
COMMIT;
BEGIN;
SELECT default_parameter_cursor(NULL);
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
