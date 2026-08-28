DROP VIEW IF EXISTS differential_operator_view CASCADE;
DROP OPERATOR CLASS IF EXISTS differential_int_class USING btree CASCADE;
DROP OPERATOR FAMILY IF EXISTS differential_int_family USING btree CASCADE;
DROP OPERATOR IF EXISTS === (integer, integer) CASCADE;
DROP OPERATOR IF EXISTS ## (integer, integer) CASCADE;
DROP OPERATOR IF EXISTS @@ (integer, integer) CASCADE;
DROP FUNCTION IF EXISTS differential_int_same(integer, integer) CASCADE;
DROP FUNCTION IF EXISTS differential_int_compare(integer, integer) CASCADE;
DROP TYPE IF EXISTS differential_mood CASCADE;

CREATE TYPE differential_mood AS ENUM ('low', 'high');
CREATE FUNCTION differential_mood_text(differential_mood)
RETURNS text LANGUAGE SQL IMMUTABLE
RETURN CASE WHEN $1 = 'low' THEN 'low-cast' ELSE 'high-cast' END;
CREATE CAST (differential_mood AS text)
WITH FUNCTION differential_mood_text(differential_mood) AS ASSIGNMENT;

SELECT 'low'::differential_mood::text;
SELECT castmethod, castcontext, castfunc <> 0
FROM pg_cast
WHERE castsource = 'differential_mood'::regtype
  AND casttarget = 'text'::regtype;

CREATE FUNCTION differential_int_same(integer, integer)
RETURNS boolean LANGUAGE SQL IMMUTABLE RETURN $1 = $2;
CREATE FUNCTION differential_int_compare(integer, integer)
RETURNS integer LANGUAGE SQL IMMUTABLE
RETURN CASE WHEN $1 < $2 THEN -1 WHEN $1 > $2 THEN 1 ELSE 0 END;
CREATE OPERATOR === (
  FUNCTION = differential_int_same,
  LEFTARG = integer,
  RIGHTARG = integer,
  HASHES,
  MERGES
);
CREATE OPERATOR ## (
  FUNCTION = differential_int_same,
  LEFTARG = integer,
  RIGHTARG = integer,
  COMMUTATOR = OPERATOR(public.@@)
);

SELECT 1 === 1, 1 OPERATOR(public.===) 2;
SELECT oprkind, oprcanmerge, oprcanhash, oprresult::regtype::text,
       pg_typeof(oprcode)::text, oprcode::text,
       oprrest::regprocedure::text, oprjoin::regprocedure::text
FROM pg_operator
WHERE oprname = '===' AND oprleft = 'integer'::regtype
ORDER BY oid;
SELECT oprresult = 0, oprcode = 0, oprcom <> 0
FROM pg_operator
WHERE oprname = '@@' AND oprleft = 'integer'::regtype;

CREATE OPERATOR @@ (
  FUNCTION = differential_int_same,
  LEFTARG = integer,
  RIGHTARG = integer
);
SELECT left_operator.oprcom = right_operator.oid,
       right_operator.oprcom = left_operator.oid
FROM pg_operator AS left_operator, pg_operator AS right_operator
WHERE left_operator.oprname = '##' AND right_operator.oprname = '@@'
  AND left_operator.oprleft = 'integer'::regtype
  AND right_operator.oprleft = 'integer'::regtype;

CREATE OPERATOR FAMILY differential_int_family USING btree;
ALTER OPERATOR FAMILY differential_int_family USING btree ADD
  OPERATOR 3 === (integer, integer),
  FUNCTION 1 (integer, integer)
    differential_int_compare(integer, integer);
SELECT amopstrategy, amoppurpose, amoplefttype::regtype::text,
       amoprighttype::regtype::text
FROM pg_amop
WHERE amopfamily = (
  SELECT oid FROM pg_opfamily WHERE opfname = 'differential_int_family'
)
ORDER BY amopstrategy;
SELECT amprocnum, amproclefttype::regtype::text,
       amprocrighttype::regtype::text, pg_typeof(amproc)::text, amproc::text
FROM pg_amproc
WHERE amprocfamily = (
  SELECT oid FROM pg_opfamily WHERE opfname = 'differential_int_family'
)
ORDER BY amprocnum;
ALTER OPERATOR FAMILY differential_int_family USING btree DROP
  OPERATOR 3 (integer, integer),
  FUNCTION 1 (integer, integer);

CREATE OPERATOR CLASS differential_int_class FOR TYPE integer USING btree
  FAMILY differential_int_family AS
  OPERATOR 3 ===,
  FUNCTION 1 differential_int_compare(integer, integer);
SELECT operator_class.opcname, operator_class.opcdefault,
       operator_class.opcintype::regtype::text, operator_family.opfname
FROM pg_opclass AS operator_class
JOIN pg_opfamily AS operator_family
  ON operator_family.oid = operator_class.opcfamily
WHERE operator_class.opcname = 'differential_int_class';

ALTER OPERATOR CLASS differential_int_class USING btree
  RENAME TO differential_int_class_moved;
ALTER OPERATOR CLASS differential_int_class_moved USING btree
  RENAME TO differential_int_class;
ALTER OPERATOR FAMILY differential_int_family USING btree
  RENAME TO differential_int_family_moved;
ALTER OPERATOR FAMILY differential_int_family_moved USING btree
  RENAME TO differential_int_family;
DROP OPERATOR CLASS differential_int_class USING btree;
DROP OPERATOR FAMILY differential_int_family USING btree;

CREATE VIEW differential_operator_view AS
SELECT 1 OPERATOR(public.===) 1 AS equivalent;
SELECT equivalent FROM differential_operator_view;
DROP OPERATOR === (integer, integer);
DROP OPERATOR === (integer, integer) CASCADE;
SELECT count(*) FROM pg_views WHERE viewname = 'differential_operator_view';

DROP OPERATOR ## (integer, integer);
DROP OPERATOR @@ (integer, integer);
DROP CAST (differential_mood AS text);
DROP FUNCTION differential_mood_text(differential_mood);
DROP FUNCTION differential_int_same(integer, integer);
DROP FUNCTION differential_int_compare(integer, integer);
DROP TYPE differential_mood;
