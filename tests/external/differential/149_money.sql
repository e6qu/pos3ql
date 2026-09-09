-- PostgreSQL 18 money: exact cents, locale-shaped text, arithmetic,
-- aggregates, arrays, indexes, and stable catalog identity.
CREATE TABLE money_values (
  id integer PRIMARY KEY,
  amount money UNIQUE,
  history money[]
);

INSERT INTO money_values VALUES
  (1, '$1,234.565', ARRAY['$1.23'::money, NULL, '-$4.56'::money]),
  (2, '(12.345)', ARRAY[]::money[]),
  (3, NULL, NULL);

SELECT id, amount, history::text, pg_typeof(amount), pg_typeof(history)
FROM money_values
ORDER BY amount NULLS LAST;

SELECT 10::money + 2.50::money,
       10::money - 2.50::money,
       10::money / 4::money,
       10::money * 3::smallint,
       3::integer * 10::money,
       10::money / 4::bigint,
       1.01::money * 1.5::double precision,
       1.01::money / 2::double precision;

SELECT 1.005::numeric::money,
       (-1.005)::numeric::money,
       12.34::money::numeric,
       cash_words(1234.56::money),
       cash_cmp(1::money, 2::money),
       cash_send('-1.23'::money);

SELECT ''::money,
       '$ 1.2'::money,
       '1.2 $'::money,
       '$-1.2'::money,
       '1.2-$'::money,
       '( $1.2 )$'::money;

SELECT cash_words(value)
FROM (VALUES (0::money), (1.01::money), (1000::money),
             (1101::money), (1200::money), ('-0.01'::money)) AS words(value);

SELECT 0.01::money * 0.5::double precision,
       0.03::money * 0.5::double precision,
       0.05::money / 2::double precision;

-- PostgreSQL's upstream money regression boundaries: text rounding, exact
-- integer division above float precision, every arithmetic width, and
-- documented extrema.
SELECT '$123.451'::money, '$123.454'::money, '$123.455'::money,
       '$123.456'::money, '$123.459'::money,
       '(1)'::money, '($123,456.78)'::money;
SELECT '-92233720368547758.08'::money,
       '92233720368547758.07'::money;
SELECT '878.08'::money / 11::double precision,
       '878.08'::money / 11::real,
       '878.08'::money / 11::bigint,
       '878.08'::money / 11::integer,
       '878.08'::money / 11::smallint;
SELECT '90000000000000099.00'::money / 10::bigint,
       '90000000000000099.00'::money / 10::integer,
       '90000000000000099.00'::money / 10::smallint;
SELECT 1234567890::int4::money,
       12345678901234567::int8::money,
       12345678901234567::numeric::money,
       (-1234567890)::int4::money,
       (-12345678901234567)::int8::money,
       (-12345678901234567)::numeric::money;
SELECT '12345678901234567'::money::numeric,
       '-12345678901234567'::money::numeric,
       '92233720368547758.07'::money::numeric,
       '-92233720368547758.08'::money::numeric;
SELECT 123::money = '$123.00',
       123::money != '$124.00',
       123::money <= '$123.00',
       123::money >= '$123.00',
       123::money < '$124.00',
       123::money > '$122.00',
       cashlarger(123::money, '$124.00'),
       cashsmaller(123::money, '$124.00');
SELECT cash_mul_int4(1::money, 2::smallint),
       cash_mul_int8(1::money, 2::integer),
       cash_mul_flt4(1::money, 2::integer),
       cash_mul_flt8(1::money, 2::numeric);

SELECT sum(amount), min(amount), max(amount) FROM money_values;
SELECT id FROM money_values WHERE amount >= 0::money ORDER BY amount;

SELECT oid, typname, typlen, typcategory, typarray, typelem, typinput, typoutput
FROM pg_type WHERE oid IN (790, 791) ORDER BY oid;

SELECT oid, proname, prorettype, proargtypes, provolatile, proisstrict, prokind
FROM pg_proc
WHERE oid IN (377, 846, 847, 848, 862, 863, 864, 865, 866, 867,
              886, 887, 888, 889, 890, 891, 892, 893, 894, 895, 896, 897,
              898, 899, 919, 935, 2112, 2125, 2141, 2492, 2493, 3344,
              3345, 3399, 3811, 3812, 3822, 3823, 3824)
ORDER BY oid;

SELECT oid, oprname, oprleft, oprright, oprresult, oprcode, oprcom, oprnegate,
       oprcanmerge, oprcanhash
FROM pg_operator
WHERE oid IN (843, 844, 845, 900, 901, 902, 903, 904, 905, 906, 907,
              908, 909, 912, 913, 914, 915, 916, 917, 918, 3346, 3347,
              3349, 3825)
ORDER BY oid;

SELECT castsource, casttarget, castfunc, castcontext, castmethod
FROM pg_cast WHERE castsource = 790 OR casttarget = 790
ORDER BY castsource, casttarget;

SELECT opcname, opcfamily, opcintype, opcdefault
FROM pg_opclass WHERE oid = 10047;
SELECT amopstrategy, amopopr
FROM pg_amop WHERE amopfamily = 2099 ORDER BY amopstrategy;
SELECT amprocnum, amproc
FROM pg_amproc WHERE amprocfamily = 2099 ORDER BY amprocnum;
SELECT aggfnoid::oid, aggtransfn, aggcombinefn, aggmtransfn, aggminvtransfn,
       aggsortop, aggtranstype, aggmtranstype
FROM pg_aggregate WHERE aggfnoid IN (2112, 2125, 2141)
ORDER BY aggfnoid;

COPY (SELECT amount, history FROM money_values ORDER BY id)
TO STDOUT (FORMAT text);

SELECT 'not money'::money;
SELECT 92233720368547758.08::numeric::money;
SELECT '-92233720368547758.09'::money;
SELECT '-92233720368547758.085'::money;
SELECT '92233720368547758.075'::money;
SELECT '92233720368547758.07'::money + '0.01'::money;
SELECT '-92233720368547758.08'::money - '0.01'::money;
SELECT '92233720368547758.07'::money * 2::double precision;
SELECT '-1'::money / 1.175494e-38::real;
SELECT '92233720368547758.07'::money * 2::integer;
SELECT '-92233720368547758.08'::money * -1::bigint;
SELECT 1::money / 0::integer;
SELECT 42::money * 'Infinity'::double precision;
SELECT 42::money * '-Infinity'::double precision;
SELECT 42::money * 'NaN'::real;
SELECT cash_mul_int2(1::money, 2::integer);
SELECT avg(amount) FROM money_values;
