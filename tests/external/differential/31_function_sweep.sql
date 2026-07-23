-- Systematic sweep of scalar functions with no prior corpus coverage: one
-- canonical call (and one edge) per function, so every dispatch arm is
-- exercised against real PostgreSQL rather than only the ones bugs happened to
-- land on. Sections mirror the dispatch tables.

-- trigonometry and hyperbolics
SELECT acos(1), asin(0), atan(1), atan2(1, 1);
SELECT cos(0), sin(0), tan(0), cot(1);
SELECT acosh(1), asinh(0), atanh(0);
SELECT cosh(0), sinh(0), tanh(0);
SELECT degrees(pi()), radians(180);

-- exponentials, roots, rounding
SELECT exp(0), exp(1);
SELECT ceil(1.4), ceiling(1.4), floor(1.6), sign(-3), sign(0);
SELECT trunc(1.999), trunc(1.987, 2), round(2.5), round(-2.5);
SELECT power(2, 10), pow(2, 0.5), sqrt(16), cbrt(8);
SELECT log(100), log10(1000), log(2, 8), ln(1);
SELECT mod(7, 3), mod(-7, 3), div(7, 3), gcd(12, 18), lcm(4, 6);
SELECT factorial(5), factorial(0);
SELECT min_scale(1.5000), scale(1.500), trim_scale(1.5000);
SELECT abs(-4.2), pi();

-- string functions
SELECT ascii('A'), chr(66), char_length('abc'), character_length('abc');
SELECT btrim('  ab  '), btrim('xxabxx', 'x'), ltrim('  ab'), rtrim('ab  ');
SELECT ltrim('xxab', 'x'), rtrim('abxx', 'x');
SELECT lpad('ab', 5), lpad('ab', 5, '*'), rpad('ab', 5, '*'), lpad('abcdef', 3);
SELECT concat('a', 1, NULL, 'b'), concat_ws(',', 'a', NULL, 'b');
SELECT initcap('hello wORLD of sql');
SELECT repeat('ab', 3), repeat('x', 0), reverse('abc');
SELECT split_part('a,b,c', ',', 2), split_part('a,b,c', ',', 9), split_part('a,b,c', ',', -1);
SELECT starts_with('abcdef', 'abc'), starts_with('abcdef', 'z');
SELECT strpos('high', 'ig'), strpos('high', 'z');
SELECT translate('12345', '143', 'ax');
SELECT overlay('Txxxxas' placing 'hom' from 2 for 4);
SELECT position('om' in 'Thomas');
SELECT md5('abc');
SELECT to_hex(255), to_hex(0);
SELECT format('Hello %s, %s!', 'world', 42);
SELECT format('%I.%L', 'my col', E'O''Reilly');
SELECT left('abcde', 2), right('abcde', 2), left('abcde', -2);

-- datetime constructors and truncation
SELECT date_trunc('hour', timestamp '2020-02-03 04:05:06');
SELECT date_trunc('month', timestamp '2020-02-03 04:05:06');
SELECT date_trunc('year', date '2020-02-03');
SELECT make_date(2020, 2, 3), make_time(4, 5, 6.5);
SELECT make_timestamp(2020, 2, 3, 4, 5, 6.5);
SELECT to_date('2020-02-03', 'YYYY-MM-DD');
SELECT to_number('12,454.8-', '99G999D9S');

-- arrays
SELECT array_lower(ARRAY[1,2,3], 1), array_upper(ARRAY[1,2,3], 1);
SELECT array_position(ARRAY['a','b','c'], 'b'), array_position(ARRAY['a','b'], 'z');
SELECT array_to_string(ARRAY[1, 2, 3, NULL, 5], ',', '*');
SELECT array_to_string(ARRAY[1,2,3], '~');

-- json
SELECT json_array_length('[1,2,3]'), jsonb_array_length('[1,2,[3,4]]');
SELECT json_extract_path('{"a":{"b":"c"}}', 'a', 'b');
SELECT json_extract_path_text('{"a":{"b":"c"}}', 'a', 'b');
SELECT json_strip_nulls('{"a":1,"b":null,"c":{"d":null}}');
SELECT jsonb_set_lax('{"a":1}', '{b}', NULL, true, 'use_json_null');

-- regexp additions
SELECT regexp_count('abcabc', 'a'), regexp_instr('abcdef', 'cd');

-- misc scalars
SELECT nullif(1, 1), nullif(1, 2), nullif('a', 'b');
SELECT version() LIKE 'PostgreSQL%';

-- jsonb_set_lax's null treatments, found missing by this sweep
SELECT jsonb_set_lax('{"a":1}', '{b}', NULL, true, 'delete_key');
SELECT jsonb_set_lax('{"a":1}', '{b}', NULL, true, 'return_target');
SELECT jsonb_set_lax('{"a":1}', '{b}', NULL, true, 'raise_exception');
SELECT jsonb_set_lax('{"a":1}', '{b}', NULL, true, 'bogus');
SELECT jsonb_set_lax('{"a":1}', '{b}', NULL, true);
SELECT jsonb_set_lax('{"a":1}', '{b}', '2', true, 'delete_key');
SELECT jsonb_set_lax('{"a":1,"b":2}', '{b}', NULL, true, 'delete_key');

-- json vs jsonb serialization of strip_nulls, found by this sweep
SELECT json_strip_nulls('{"a" : 1 , "b":null}');
SELECT jsonb_strip_nulls('{"a" : 1, "b":null}');

-- date_trunc over a date is a timestamptz; two-argument log is numeric only
SELECT pg_typeof(date_trunc('year', date '2020-02-03'));
SELECT log(2, 8), pg_typeof(log(2, 8));
SELECT log(2.5::float8, 8);
