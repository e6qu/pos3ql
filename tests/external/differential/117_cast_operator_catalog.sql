DROP VIEW IF EXISTS differential_operator_view CASCADE;
DROP OPERATOR CLASS IF EXISTS differential_int_class USING btree CASCADE;
DROP OPERATOR FAMILY IF EXISTS differential_int_family USING btree CASCADE;
DROP OPERATOR IF EXISTS === (integer, integer) CASCADE;
DROP OPERATOR IF EXISTS ## (integer, integer) CASCADE;
DROP OPERATOR IF EXISTS @@ (integer, integer) CASCADE;
DROP OPERATOR IF EXISTS !! (NONE, integer) CASCADE;
DROP FUNCTION IF EXISTS differential_int_prefix(integer) CASCADE;
DROP FUNCTION IF EXISTS differential_int_same(integer, integer) CASCADE;
DROP FUNCTION IF EXISTS differential_int_compare(integer, integer) CASCADE;
DROP TYPE IF EXISTS differential_mood CASCADE;

CREATE TYPE differential_mood AS ENUM ('low', 'high');
CREATE FUNCTION differential_mood_text(differential_mood, integer)
RETURNS varchar LANGUAGE SQL IMMUTABLE
RETURN CASE WHEN $2 = -1 THEN
  CASE WHEN $1 = 'low' THEN 'low-cast' ELSE 'high-cast' END
ELSE 'bad-typmod' END;
CREATE CAST (differential_mood AS text)
WITH FUNCTION differential_mood_text AS ASSIGNMENT;
COMMENT ON CAST (differential_mood AS text) IS 'cast catalog comment';

SELECT 'low'::differential_mood::text;
SELECT castmethod, castcontext, castfunc <> 0
FROM pg_cast
WHERE castsource = 'differential_mood'::regtype
  AND casttarget = 'text'::regtype;
SELECT obj_description(oid, 'pg_cast')
FROM pg_cast
WHERE castsource = 'differential_mood'::regtype
  AND casttarget = 'text'::regtype;

CREATE FUNCTION differential_int_same(integer, integer)
RETURNS boolean LANGUAGE SQL IMMUTABLE RETURN $1 % 10 = $2 % 10;
CREATE FUNCTION differential_int_compare(integer, integer)
RETURNS integer LANGUAGE SQL IMMUTABLE
RETURN CASE WHEN $1 % 10 < $2 % 10 THEN -1
            WHEN $1 % 10 > $2 % 10 THEN 1 ELSE 0 END;
CREATE FUNCTION differential_int_prefix(integer)
RETURNS integer LANGUAGE SQL IMMUTABLE RETURN -$1;
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
CREATE OPERATOR !! (
  FUNCTION = differential_int_prefix,
  RIGHTARG = integer
);
COMMENT ON OPERATOR === (integer, integer) IS 'operator catalog comment';

SELECT 1 === 1, 1 OPERATOR(public.===) 2;
SELECT !! 4, OPERATOR(public.!!) 5;
SELECT obj_description(oid, 'pg_operator')
FROM pg_operator
WHERE oprname = '===' AND oprleft = 'integer'::regtype;
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
COMMENT ON OPERATOR FAMILY differential_int_family USING btree
  IS 'operator family catalog comment';
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
COMMENT ON OPERATOR CLASS differential_int_class USING btree
  IS 'operator class catalog comment';
SELECT operator_class.opcname, operator_class.opcdefault,
       operator_class.opcintype::regtype::text, operator_family.opfname
FROM pg_opclass AS operator_class
JOIN pg_opfamily AS operator_family
  ON operator_family.oid = operator_class.opcfamily
WHERE operator_class.opcname = 'differential_int_class';
SELECT
  (SELECT count(*) FROM pg_depend AS dependency
   JOIN pg_amop AS member ON member.oid = dependency.objid
   WHERE dependency.classid = 'pg_amop'::regclass
     AND dependency.refclassid = 'pg_opclass'::regclass
     AND dependency.refobjid = (
       SELECT oid FROM pg_opclass WHERE opcname = 'differential_int_class'
     ) AND dependency.deptype = 'i') AS operator_dependencies,
  (SELECT count(*) FROM pg_depend AS dependency
   JOIN pg_amproc AS member ON member.oid = dependency.objid
   WHERE dependency.classid = 'pg_amproc'::regclass
     AND dependency.refclassid = 'pg_opclass'::regclass
     AND dependency.refobjid = (
       SELECT oid FROM pg_opclass WHERE opcname = 'differential_int_class'
     ) AND dependency.deptype = 'i') AS function_dependencies;

CREATE TABLE differential_operator_class_values(value integer);
CREATE UNIQUE INDEX differential_operator_class_values_idx
  ON differential_operator_class_values (value differential_int_class);
SELECT count(*) FROM pg_depend
WHERE classid = 'pg_class'::regclass
  AND objid = 'differential_operator_class_values_idx'::regclass
  AND refclassid = 'pg_opclass'::regclass
  AND refobjid = (
    SELECT oid FROM pg_opclass WHERE opcname = 'differential_int_class'
  ) AND deptype = 'n';
INSERT INTO differential_operator_class_values VALUES (1);
INSERT INTO differential_operator_class_values VALUES (11);
SELECT pg_get_indexdef('differential_operator_class_values_idx'::regclass);

ALTER OPERATOR CLASS differential_int_class USING btree
  RENAME TO differential_int_class_moved;
ALTER OPERATOR CLASS differential_int_class_moved USING btree
  RENAME TO differential_int_class;
SELECT obj_description(oid, 'pg_opclass')
FROM pg_opclass
WHERE opcname = 'differential_int_class';
SELECT count(*) FROM pg_indexes
WHERE indexname = 'differential_operator_class_values_idx';
ALTER OPERATOR FAMILY differential_int_family USING btree
  RENAME TO differential_int_family_moved;
ALTER OPERATOR FAMILY differential_int_family_moved USING btree
  RENAME TO differential_int_family;
SELECT obj_description(oid, 'pg_opfamily')
FROM pg_opfamily
WHERE opfname = 'differential_int_family';
DROP OPERATOR CLASS differential_int_class USING btree;
DROP OPERATOR CLASS differential_int_class USING btree CASCADE;
SELECT count(*) FROM pg_indexes
WHERE indexname = 'differential_operator_class_values_idx';
DROP OPERATOR FAMILY differential_int_family USING btree;

CREATE VIEW differential_operator_view AS
SELECT 1 OPERATOR(public.===) 1 AS equivalent;
SELECT equivalent FROM differential_operator_view;
DROP OPERATOR === (integer, integer);
DROP OPERATOR === (integer, integer) CASCADE;
SELECT count(*) FROM pg_views WHERE viewname = 'differential_operator_view';

DROP OPERATOR ## (integer, integer);
DROP OPERATOR @@ (integer, integer);
DROP OPERATOR !! (NONE, integer);
DROP CAST (differential_mood AS text);
DROP FUNCTION differential_mood_text(differential_mood, integer);
DROP FUNCTION differential_int_prefix(integer);
DROP FUNCTION differential_int_same(integer, integer);
DROP FUNCTION differential_int_compare(integer, integer);
DROP TYPE differential_mood;

CREATE TABLE differential_constraint_comment (
  value integer,
  CONSTRAINT differential_constraint_first CHECK (value > 0),
  CONSTRAINT differential_constraint_second CHECK (value < 100)
);
COMMENT ON CONSTRAINT differential_constraint_second
  ON differential_constraint_comment IS 'constraint catalog comment';
ALTER TABLE differential_constraint_comment
  DROP CONSTRAINT differential_constraint_first;
SELECT obj_description(oid, 'pg_constraint')
FROM pg_constraint
WHERE conrelid = 'differential_constraint_comment'::regclass
  AND conname = 'differential_constraint_second';
ALTER TABLE differential_constraint_comment
  RENAME CONSTRAINT differential_constraint_second
  TO differential_constraint_renamed;
SELECT obj_description(oid, 'pg_constraint')
FROM pg_constraint
WHERE conrelid = 'differential_constraint_comment'::regclass
  AND conname = 'differential_constraint_renamed';
ALTER TABLE differential_constraint_comment
  DROP CONSTRAINT differential_constraint_renamed;
DROP TABLE differential_constraint_comment;
