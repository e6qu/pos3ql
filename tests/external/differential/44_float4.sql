-- real/float4 as a genuine single-precision type, not float8 in disguise.
-- Every value is rounded to f32 on input, arithmetic between two reals stays
-- single precision, and output uses PostgreSQL's float4out rule (shortest f32
-- round-trip; fixed notation for decimal exponents in [-4, 6), scientific
-- otherwise). Pinned against PostgreSQL 18. real output goes through
-- PostgreSQL's own Ryū (its non-STRICTLY_SHORTEST boundary handling), so even
-- shortest-representation boundary cases match exactly.
DROP TABLE IF EXISTS f4;

-- Boundary cases where Rust's shortest-float and PostgreSQL's Ryū disagree
-- (PostgreSQL keeps the extra digit): these must match PostgreSQL now.
SELECT 87535936::real, 59326392::real, 3188318.25::real, 1.0000005e6::real;

-- Output: the fixed/scientific window differs from float8's [-4, 15). Values
-- that need f32 rounding (12345678, 0.100000001, 16777217) are the whole point.
SELECT 12345678::real, 1234567::real, 123456::real, 1000000::real, 999999::real;
SELECT 0.0001::real, 1e-4::real, 1e-5::real, 100000::real, 1000000.5::real;
SELECT 0.1::real, 0.100000001::real, 16777216::real, 16777217::real, 0.3::real;
SELECT (-0.0)::real, 0.0::real, 3.4e38::real, 1.2e-38::real, 1e-45::real;
SELECT 'infinity'::real, '-inf'::real, 'nan'::real, 'Infinity'::float8::real;

-- pg_typeof and OID identity (real is its own type, OID 700 — not float8).
SELECT pg_typeof(1.5::real), pg_typeof(1.5::real) = 'real'::regtype, pg_typeof(1.5::real) = pg_typeof(1.5::float8);

-- Casts to real, including out-of-range (three source shapes, three messages).
SELECT 100::real, 9223372036854775807::bigint::real, 1.5::float8::real, 2.5::numeric::real;
SELECT '3.5e38'::real;
SELECT (1e40::float8)::real;
SELECT (1e40::numeric)::real;
SELECT 'abc'::real;

-- Casts from real: int rounds ties-to-even, float8 widens, numeric/text exact.
SELECT 2.5::real::int, 3.5::real::int, 0.5::real::int, (-2.5::real)::int;
SELECT 1.5::real::float8, 0.100000001::real::float8, 1.1::real::numeric, 12345678::real::text;

-- Arithmetic promotion: real op real -> real (single precision); real mixed
-- with int/float8/numeric -> double precision; no modulo for real.
SELECT pg_typeof(1::real + 2::real), pg_typeof(1::real + 2), pg_typeof(1::real + 2.0::float8), pg_typeof(1::real + 2::numeric);
SELECT (0.1::real + 0.2::real), (1::real / 3::real), pg_typeof(1::real * 2::real);
SELECT pg_typeof(-(1.5::real)), (-(2.5::real)), abs(-1.5::real), pg_typeof(abs(-1.5::real));
SELECT 5::real % 2::real;
SELECT 5.0::float8 % 2.0::float8;

-- Rounding/sign widen to double precision (no real overload in PostgreSQL).
SELECT floor(1.7::real), ceil(1.2::real), round(2.5::real), trunc(1.9::real), sign(-3::real);
SELECT pg_typeof(floor(1.7::real)), pg_typeof(sign(1::real)), sqrt(2::real), pg_typeof(sqrt(2::real));

-- Aggregates: sum(real) accumulates in f32 (adds past 2^24 are lost, exactly
-- as PostgreSQL's float4 sum); avg/variance widen to double precision.
CREATE TABLE f4 (id int, r real);
INSERT INTO f4 VALUES (1, 16777216), (2, 1), (3, 1), (4, 0.1), (5, 0.2), (6, 0.3);
SELECT sum(r), pg_typeof(sum(r)) FROM f4 WHERE id <= 3;
SELECT sum(r)::text, pg_typeof(sum(r)) FROM f4 WHERE id >= 4;
SELECT avg(r), pg_typeof(avg(r)), pg_typeof(var_pop(r)), pg_typeof(stddev(r)) FROM f4 WHERE id >= 4;
SELECT min(r), max(r), pg_typeof(max(r)) FROM f4;

-- greatest/least and UNION follow the tower: real beats int and numeric,
-- float8 beats real.
SELECT pg_typeof(greatest(1::real, 2)), pg_typeof(greatest(1::real, 2::numeric)), pg_typeof(greatest(1::real, 2::float8));
SELECT greatest(1.5::real, 3::int), least(1.5::real, 0.5::real);
SELECT pg_typeof(x) FROM (SELECT 1::real UNION SELECT 2::int) t(x) LIMIT 1;
SELECT pg_typeof(x) FROM (SELECT 1::real UNION SELECT 2::numeric) t(x) LIMIT 1;
SELECT pg_typeof(x) FROM (SELECT 1::real UNION SELECT 2::float8) t(x) LIMIT 1;

-- real[] is its own array type (OID 1021), stored and read back as f32.
SELECT ARRAY[1.5::real, 2.5::real], pg_typeof(ARRAY[1.5::real]);
SELECT '{0.1,16777217}'::real[], ('{0.1,16777217}'::real[])[2], pg_typeof('{1}'::real[]);

-- JSON renders real (and smallint) as bare numbers, not quoted strings.
SELECT to_json(1.5::real), to_json(7::smallint), row_to_json(t) FROM (SELECT 1.5::real AS a, 7::int2 AS b) t;
SELECT json_build_object('r', 2.5::real, 's', 3::smallint);

-- Comparisons and ordering treat real as its numeric value.
SELECT r, r > 0.15::real, r = 0.1::real FROM f4 WHERE id IN (4,5) ORDER BY r;
SELECT count(*) FROM f4 WHERE r < 1.0;

DROP TABLE f4;
