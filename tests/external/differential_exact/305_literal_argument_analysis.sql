-- Unknown-type literals convert while the statement is analysed: a builtin
-- function argument with one candidate signature, NULLIF's operand, an array
-- constructor's element and an element of a constant array cast. A failed
-- conversion is positioned at the literal even over no rows.
create table t305 (a int);
select int4pl(a, 'p') from t305;
select * from t305 where int4pl(a, 'p') > 0;
select ARRAY['1','x']::int[];
select ARRAY['1','x']::int[] from t305;
select ARRAY[['1'],['x']]::int[];
select '{1,x}'::int[];
select lower(1, 'x');
select coalesce(a, 'x') from t305;
select greatest(a, 'x') from t305;
select nullif(a, 'x') from t305;
select a in (1, 'x') from t305;
select case when true then a else 'x' end from t305;
select array[a, 'x'] from t305;
select int4pl(a, '2'), nullif(a, '3'), array[a, '4'] from t305;
insert into t305 values (1);
select int4pl(a, '2'), nullif(a, '1'), array[a, '4'] from t305;
