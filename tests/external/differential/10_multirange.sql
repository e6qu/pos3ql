-- multirange literals and canonicalization (sort, merge overlapping/adjacent, drop empty)
SELECT '{[1,3),[5,7)}'::int4multirange;
SELECT '{[5,7),[1,3)}'::int4multirange;
SELECT '{[1,3),[2,5)}'::int4multirange;
SELECT '{[1,3),[3,5)}'::int4multirange;
SELECT '{}'::int4multirange;
SELECT '{[1,1)}'::int4multirange;
SELECT '{[1,10),[2,3)}'::int4multirange;
SELECT '{[1.5,3.5),[10,20)}'::nummultirange;
-- range -> multirange cast
SELECT int4range(1,5)::int4multirange, 'empty'::int4range::int4multirange;
-- comparison / equality / ordering
SELECT '{[1,3)}'::int4multirange = '{[1,3)}'::int4multirange;
SELECT '{[1,3)}'::int4multirange < '{[1,5)}'::int4multirange;
SELECT '{[1,3)}'::int4multirange < '{[1,3),[5,7)}'::int4multirange;
-- constructors
SELECT int4multirange(int4range(1,3), int4range(5,7));
SELECT int4multirange(int4range(1,3), int4range(2,6));
SELECT int4multirange();
SELECT nummultirange(numrange(1.5, 3.5));
-- lower / upper / isempty
SELECT lower('{[2,5),[8,10)}'::int4multirange), upper('{[2,5),[8,10)}'::int4multirange);
SELECT isempty('{}'::int4multirange), isempty('{[1,2)}'::int4multirange);
-- containment / overlap
SELECT '{[1,5),[10,15)}'::int4multirange @> 3;
SELECT '{[1,5),[10,15)}'::int4multirange @> 7;
SELECT '{[1,5),[10,15)}'::int4multirange @> int4range(2,4);
SELECT '{[1,5),[10,15)}'::int4multirange @> '{[2,4),[11,12)}'::int4multirange;
SELECT int4range(2,4) <@ '{[1,5)}'::int4multirange;
SELECT '{[1,5)}'::int4multirange && '{[4,8)}'::int4multirange;
SELECT '{[1,5)}'::int4multirange && '{[6,8)}'::int4multirange;
-- union / intersection / difference
SELECT '{[1,5)}'::int4multirange + '{[10,15)}'::int4multirange;
SELECT '{[1,10)}'::int4multirange * '{[5,15)}'::int4multirange;
SELECT '{[1,10)}'::int4multirange - '{[3,5)}'::int4multirange;
SELECT '{[1,20)}'::int4multirange - '{[3,5),[10,12)}'::int4multirange;
