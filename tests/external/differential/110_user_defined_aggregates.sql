DROP AGGREGATE IF EXISTS differential_total(integer);
DROP AGGREGATE IF EXISTS differential_offset(integer ORDER BY integer);
DROP AGGREGATE IF EXISTS differential_hypothetical(integer ORDER BY integer);
DROP FUNCTION IF EXISTS differential_total_state(bigint, integer);
DROP FUNCTION IF EXISTS differential_total_inverse(bigint, integer);
DROP FUNCTION IF EXISTS differential_total_final(bigint);
DROP FUNCTION IF EXISTS differential_offset_final(bigint, integer);
DROP AGGREGATE IF EXISTS differential_first(anyelement);
DROP AGGREGATE IF EXISTS differential_array_first(anyarray);
DROP FUNCTION IF EXISTS differential_first_state(anyelement, anyelement);
DROP FUNCTION IF EXISTS differential_array_first_state(anyarray, anyarray);
DROP AGGREGATE IF EXISTS differential_tick(*);
DROP FUNCTION IF EXISTS differential_tick_state(bigint);
DROP TABLE IF EXISTS differential_aggregate_input;

CREATE TABLE differential_aggregate_input (group_id integer, value integer);
INSERT INTO differential_aggregate_input VALUES
  (1, 3), (1, 1), (1, 2), (1, NULL), (2, 5);

CREATE FUNCTION differential_total_state(state bigint, value integer)
RETURNS bigint LANGUAGE SQL IMMUTABLE
AS 'SELECT coalesce(state, 0) + coalesce(value, 0)';
CREATE FUNCTION differential_total_inverse(state bigint, value integer)
RETURNS bigint LANGUAGE SQL IMMUTABLE
AS 'SELECT state - coalesce(value, 0)';
CREATE FUNCTION differential_total_final(state bigint)
RETURNS bigint LANGUAGE SQL IMMUTABLE
AS 'SELECT state * 2';
CREATE FUNCTION differential_offset_final(state bigint, direct integer)
RETURNS bigint LANGUAGE SQL IMMUTABLE
AS 'SELECT state + direct';
CREATE FUNCTION differential_first_state(state anyelement, value anyelement)
RETURNS anyelement LANGUAGE SQL IMMUTABLE
AS 'SELECT coalesce(state, value)';
CREATE FUNCTION differential_array_first_state(state anyarray, value anyarray)
RETURNS anyarray LANGUAGE SQL IMMUTABLE
AS 'SELECT coalesce(state, value)';
CREATE FUNCTION differential_tick_state(state bigint)
RETURNS bigint LANGUAGE SQL IMMUTABLE
AS 'SELECT coalesce(state, 0) + 1';

CREATE AGGREGATE differential_total(integer) (
  SFUNC = differential_total_state,
  STYPE = bigint,
  FINALFUNC = differential_total_final,
  MSFUNC = differential_total_state,
  MINVFUNC = differential_total_inverse,
  MSTYPE = bigint,
  MFINALFUNC = differential_total_final,
  PARALLEL = SAFE
);
CREATE AGGREGATE differential_offset(integer ORDER BY integer) (
  SFUNC = differential_total_state,
  STYPE = bigint,
  INITCOND = '0',
  FINALFUNC = differential_offset_final
);
CREATE AGGREGATE differential_hypothetical(integer ORDER BY integer) (
  SFUNC = differential_total_state,
  STYPE = bigint,
  INITCOND = '0',
  FINALFUNC = differential_offset_final,
  HYPOTHETICAL
);
CREATE AGGREGATE differential_first(anyelement) (
  SFUNC = differential_first_state,
  STYPE = anyelement
);
CREATE AGGREGATE differential_array_first(anyarray) (
  SFUNC = differential_array_first_state,
  STYPE = anyarray
);
CREATE AGGREGATE differential_tick (
  BASETYPE = any,
  SFUNC = differential_tick_state,
  STYPE = bigint,
  INITCOND = '0'
);

SELECT group_id, differential_total(value)
  FROM differential_aggregate_input GROUP BY group_id ORDER BY group_id;
SELECT differential_total(value), differential_total(DISTINCT value),
       differential_total(value) FILTER (WHERE value > 2)
  FROM differential_aggregate_input;
SELECT value,
       differential_total(value) OVER (
         ORDER BY value ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)
  FROM differential_aggregate_input ORDER BY value;
SELECT differential_offset(10) WITHIN GROUP (ORDER BY value DESC),
       differential_hypothetical(10) WITHIN GROUP (ORDER BY value)
  FROM differential_aggregate_input;
SELECT differential_offset(10) WITHIN GROUP (ORDER BY value)
  FROM differential_aggregate_input WHERE false;
SELECT differential_first(value), differential_first(group_id::text),
       pg_typeof(differential_first(value)),
       differential_array_first(ARRAY[value])::text
  FROM differential_aggregate_input;
SELECT differential_tick(*) FROM differential_aggregate_input;
SELECT p.prokind, p.prorettype::regtype::text, p.proparallel,
       a.aggkind, a.aggnumdirectargs, a.aggtranstype::regtype::text,
       a.agginitval
  FROM pg_proc p JOIN pg_aggregate a ON a.aggfnoid = p.oid
 WHERE p.proname IN ('differential_total', 'differential_offset',
                     'differential_hypothetical')
 ORDER BY p.proname;
SELECT p.proname, p.prorettype::regtype::text, a.aggtranstype::regtype::text
  FROM pg_proc p JOIN pg_aggregate a ON a.aggfnoid = p.oid
 WHERE p.proname IN ('differential_first', 'differential_array_first')
 ORDER BY p.proname;

CREATE OR REPLACE AGGREGATE differential_total(integer) (
  SFUNC = differential_total_state,
  STYPE = bigint,
  INITCOND = '10',
  PARALLEL = RESTRICTED
);
SELECT differential_total(value) FROM differential_aggregate_input;
SELECT proparallel, agginitval
  FROM pg_proc p JOIN pg_aggregate a ON a.aggfnoid = p.oid
 WHERE p.proname = 'differential_total';

DROP AGGREGATE differential_total(integer);
DROP AGGREGATE differential_offset(integer ORDER BY integer);
DROP AGGREGATE differential_hypothetical(integer ORDER BY integer);
DROP FUNCTION differential_total_state(bigint, integer);
DROP FUNCTION differential_total_inverse(bigint, integer);
DROP FUNCTION differential_total_final(bigint);
DROP FUNCTION differential_offset_final(bigint, integer);
DROP TABLE differential_aggregate_input;
