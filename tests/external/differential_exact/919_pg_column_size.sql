-- pg_column_size is the bytes a value takes in a tuple: a fixed type's length,
-- or a varlena's header (one byte for a stored value short enough, else four)
-- and data, for the types a sort charges.
CREATE TABLE t919 (t text, n numeric, r int4range, j jsonb, a int[], b bit varying, q tsquery, p jsonpath, i int8range, m int4multirange);
INSERT INTO t919 VALUES ('abc', 1.5, '[1,5)', '{"a": 1}', '{1,2}', B'1010101010', 'fat & rat', '$.a[0]', '(,9]', '{[1,3),[5,7)}');
INSERT INTO t919 VALUES (repeat('x', 200), 123456789.123456789, 'empty', '[1, 2, 3]', ARRAY(SELECT generate_series(1, 40)), B'1', '!cat | (dog <-> bird:*A)', '$.a ? (@ == $v && exists(@.b))', '[2,4]', '{}');
SELECT pg_column_size(t), pg_column_size(n), pg_column_size(r), pg_column_size(j), pg_column_size(a), pg_column_size(b), pg_column_size(q), pg_column_size(p), pg_column_size(i), pg_column_size(m) FROM t919;
SELECT pg_column_size(t::text), pg_column_size(t || ''), pg_column_size(t919), pg_column_size(ROW(t, n)), pg_column_size(ARRAY[ROW(1, 2), ROW(3, 4)]) FROM t919;
SELECT pg_column_size('[1,5)'::int4range), pg_column_size('empty'::int4range), pg_column_size('(,5]'::int8range), pg_column_size('[2020-01-01,2021-01-01)'::daterange), pg_column_size('[1.5,22.123456789)'::numrange), pg_column_size('[2020-01-01 10:00,infinity)'::tsrange), pg_column_size('(,)'::tstzrange);
SELECT pg_column_size('{[1,3),[5,7)}'::int4multirange), pg_column_size('{}'::int4multirange), pg_column_size('{[1,3),[5,7),[9,10)}'::int8multirange), pg_column_size('{[1.5,3),[5,7)}'::nummultirange), pg_column_size('{[2020-01-01 10:00,2020-02-01)}'::tsmultirange);
SELECT pg_column_size('fat & rat'::tsquery), pg_column_size('!cat | (dog <-> bird:*A)'::tsquery), pg_column_size(''::tsquery), pg_column_size('a'::tsquery);
SELECT pg_column_size('$.a[0]'::jsonpath), pg_column_size('strict $.a ? (@.price > 10.5)'::jsonpath), pg_column_size('$.a[1 to 3, 5].b.size()'::jsonpath), pg_column_size('$ ? (@ like_regex "a.*" flag "i")'::jsonpath), pg_column_size('$.**{2 to 5}.x'::jsonpath), pg_column_size('$.a.datetime("YYYY")'::jsonpath), pg_column_size('$.a ? (!(@ > 1) || @ == "s")'::jsonpath), pg_column_size('$.a.decimal(5,2)'::jsonpath);
SELECT pg_column_size('10:20:10,14'::txid_snapshot), pg_column_size('10:20:'::pg_snapshot), pg_column_size(B'1010101010'), pg_column_size('(1,2),(3,4)'::box), pg_column_size('[(1,2),(3,4)]'::path), pg_column_size('((1,2),(3,4),(5,6))'::polygon), pg_column_size('1 2'::int2vector), pg_column_size('1 2 3'::oidvector);
