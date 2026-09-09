-- Complete the executable PostgreSQL 18 range and multirange boundary:
-- cross-family predicates, support functions, aggregates, SRF expansion,
-- catalogs, indexes, constraints, generated columns, views, and updates.
DROP VIEW IF EXISTS range_complete_view;
DROP TABLE IF EXISTS range_complete;

SELECT '{[1,3),[5,7)}'::int4multirange << '[8,10)'::int4range,
       '[0,1)'::int4range << '{[1,3),[5,7)}'::int4multirange,
       '{[8,10)}'::int4multirange >> '[5,8)'::int4range,
       '{}'::int4multirange << '[1,2)'::int4range;
SELECT '{[1,3),[5,7)}'::int4multirange &< '[2,6)'::int4range,
       '[2,6)'::int4range &> '{[1,3),[5,7)}'::int4multirange,
       '{[1,3),[5,7)}'::int4multirange -|- '[7,9)'::int4range,
       '[7,9)'::int4range -|- '{[1,3),[5,7)}'::int4multirange;

SELECT lower_inc('{[1,3),[5,7)}'::int4multirange),
       upper_inc('{[1,3),[5,7)}'::int4multirange),
       lower_inf('{(,3),[5,7)}'::int4multirange),
       upper_inf('{[1,3),[5,)}'::int4multirange),
       lower_inc('{}'::int4multirange), upper_inf('{}'::int4multirange);
SELECT range_merge('{[1,3),[5,7)}'::int4multirange),
       range_merge('{}'::int4multirange);

SELECT range_cmp('[1,3)'::int4range, '[1,4)'::int4range),
       range_eq('[1,3)'::int4range, '[1,3)'::int4range),
       range_before('[1,3)'::int4range, '[5,7)'::int4range),
       range_union('[1,3)'::int4range, '[3,7)'::int4range),
       range_intersect('[1,5)'::int4range, '[3,7)'::int4range),
       range_minus('[1,5)'::int4range, '[3,5)'::int4range);
SELECT multirange_cmp('{[1,3)}'::int4multirange, '{[1,4)}'::int4multirange),
       multirange_eq('{[1,3)}'::int4multirange, '{[1,3)}'::int4multirange),
       multirange_overlaps_range('{[1,3)}'::int4multirange, '[2,4)'::int4range),
       multirange_contains_range('{[1,5)}'::int4multirange, '[2,4)'::int4range),
       range_adjacent_multirange('[3,5)'::int4range, '{[1,3)}'::int4multirange),
       multirange_union('{[1,3)}'::int4multirange, '{[3,5)}'::int4multirange);
SELECT hash_range('[1,3)'::int4range), hash_range_extended('[1,3)'::int4range, 123),
       hash_range('[2020-01-01,2020-01-03)'::daterange),
       hash_range('[2020-01-01,2020-01-03)'::tsrange),
       hash_range('[1.25,3.50)'::numrange),
       hash_range('[-0.001,100000)'::numrange),
       hash_multirange('{[1,3),[5,7)}'::int4multirange),
       hash_multirange_extended('{[1,3),[5,7)}'::int4multirange, 123);
SELECT int4range_canonical('(1,3]'::int4range),
       int8range_canonical('(1,3]'::int8range),
       daterange_canonical('(2020-01-01,2020-01-03]'::daterange),
       int4range_subdiff(9, 2), int8range_subdiff(9::bigint, 2::bigint),
       numrange_subdiff(9.5, 2.25),
       daterange_subdiff('2020-01-09'::date, '2020-01-02'::date),
       tsrange_subdiff('2020-01-01 00:00:09'::timestamp,
                       '2020-01-01 00:00:02'::timestamp);
SELECT int4multirange(VARIADIC ARRAY['[1,3)'::int4range, '[5,7)']),
       multirange('[1,3)'::int4range);
SELECT pg_typeof(ARRAY[NULL]),
       pg_typeof(ARRAY['[1,3)'::int4range, NULL]);

SELECT range_agg(value ORDER BY value)
FROM (VALUES ('[1,3)'::int4range), ('[5,7)'), ('[2,6)'), (NULL)) input(value);
SELECT range_agg(value), range_intersect_agg(value)
FROM (VALUES ('{[1,5),[10,15)}'::int4multirange),
             ('{[3,12)}'::int4multirange), (NULL)) input(value);
SELECT range_intersect_agg(value)
FROM (VALUES ('[1,10)'::int4range), ('[3,7)'), (NULL)) input(value);
SELECT range_agg(value) FROM (SELECT NULL::int4range AS value WHERE false) input;

SELECT value, pg_typeof(value)
FROM unnest('{[1,3),[5,7)}'::int4multirange) AS expanded(value);
SELECT unnest('{[1,3),[5,7)}'::int4multirange);
SELECT * FROM unnest(ARRAY[1,2,3], '{[1,3),[5,7)}'::int4multirange);

CREATE TABLE range_complete (
  id integer PRIMARY KEY,
  span int4range NOT NULL,
  spans int4multirange NOT NULL,
  hull int4range GENERATED ALWAYS AS (range_merge(spans)) STORED,
  CHECK (spans @> span)
);
CREATE INDEX range_complete_span_btree ON range_complete USING btree (span);
CREATE INDEX range_complete_spans_btree ON range_complete USING btree (spans);
INSERT INTO range_complete (id, span, spans) VALUES
  (1, '[1,3)', '{[1,5),[10,12)}'),
  (2, '[5,8)', '{[5,9)}'),
  (3, '[20,25)', '{[20,25)}');
CREATE VIEW range_complete_view AS
SELECT id, range_merge(spans) AS hull FROM range_complete;
SELECT id, span, spans, hull FROM range_complete ORDER BY span;
SELECT id FROM range_complete WHERE span >= '[5,8)'::int4range ORDER BY id;
SELECT id FROM range_complete WHERE spans < '{[20,25)}'::int4multirange ORDER BY id;
UPDATE range_complete SET spans = spans + '{[12,15)}'::int4multirange WHERE id = 1;
SELECT * FROM range_complete_view ORDER BY id;

SELECT oid, proname, prorettype, proargtypes, proretset, proisstrict
FROM pg_proc
WHERE oid IN (1293, 3850, 3851, 3852, 3853, 3854, 3870, 4057, 4228,
              4235, 4236, 4237, 4238, 4239, 4240, 4241, 4273, 4278,
              4279, 4301, 4389, 4450, 6227)
ORDER BY oid;
SELECT aggfnoid, aggtransfn, aggfinalfn, aggcombinefn, aggtranstype
FROM pg_aggregate WHERE aggfnoid IN (4301, 4389, 4450, 6227) ORDER BY aggfnoid;
SELECT oid, oprname, oprleft, oprright, oprresult, oprcode
FROM pg_operator
WHERE oid IN (2860,2862,2868,2869,2870,2871,2872,2873,2874,
              3882,3884,3888,3889,3890,3891,3892,3893,3894,3895,3896,
              3897,3898,3899,3900,4179,4180,4198,4392,4393,4394,
              4395,4396,4397,4398,4399,4400,4539,4540)
ORDER BY oid;
SELECT 3882::regoperator, '=(anyrange,anyrange)'::regoperator::oid,
       2860::regoperator, '=(anymultirange,anymultirange)'::regoperator::oid,
       3896::regoper;
SELECT oid, opfname, opfmethod FROM pg_opfamily
WHERE oid IN (3901,3903,4199,4225) ORDER BY oid;
SELECT oid, opcname, opcfamily, opcintype, opcdefault FROM pg_opclass
WHERE oid IN (10076,10077,10080,10081) ORDER BY oid;
SELECT oid, amopfamily, amoplefttype, amoprighttype, amopstrategy, amopopr
FROM pg_amop WHERE amopfamily IN (3901,3903,4199,4225) ORDER BY oid;
SELECT oid, amprocfamily, amproclefttype, amprocrighttype, amprocnum, amproc
FROM pg_amproc WHERE amprocfamily IN (3901,3903,4199,4225) ORDER BY oid;

DROP VIEW range_complete_view;
DROP TABLE range_complete;
