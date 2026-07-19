-- Literals, operators, precedence, three-valued logic, functions.
SELECT 1, 2.5, 'text', TRUE, FALSE, NULL;
SELECT 1::bigint, '42'::int, 2.7::int, 'on'::bool, true::text, '2.5'::float8;
SELECT 1 + 2 * 3, (1 + 2) * 3, 7 / 2, 7 % 2, -5, 2147483647 + 1;
SELECT 1 < 2, 2 <= 2, 3 <> 4, 'a' || 'b' || 'c';
SELECT 0.1::float8 + 0.2::float8, 1e10::float8, 2.5::float8 * 4;
SELECT NULL AND FALSE, NULL AND TRUE, NULL OR TRUE, NULL IS NULL, 5 IS NOT NULL;
SELECT length('héllo'), upper('mIx'), lower('MiX'), abs(-7), coalesce(NULL, 'x');
SELECT 3 BETWEEN 1 AND 5, 6 BETWEEN 1 AND 5, 3 NOT BETWEEN 1 AND 5;
SELECT 2 IN (1, 2, 3), 5 IN (1, 2, 3), 5 NOT IN (1, 2, 3), 5 IN (1, NULL), 1 IN (1, NULL);
SELECT 'hello' LIKE 'h%', 'hello' LIKE '_e_lo', 'HELLO' ILIKE 'he%', 'x' NOT LIKE 'y%';
SELECT CASE WHEN 1 > 2 THEN 'a' WHEN 2 > 1 THEN 'b' ELSE 'c' END;
SELECT CASE 3 WHEN 1 THEN 'one' WHEN 3 THEN 'three' END;
SELECT CASE 3 WHEN 9 THEN 'nine' END;
-- array concatenation and its operator resolution
SELECT ARRAY[1,2] || 3, ARRAY[1,2] || ARRAY[3,4], 0 || ARRAY[1,2];
SELECT ARRAY['a','b'] || 'c'::text, ARRAY['a','b'] || '{c,d}';
SELECT ARRAY[1,2] || '{3,4}';
-- an unknown literal is cast to the array type (parsed as an array literal)
SELECT ARRAY['a','b'] || 'c';
SELECT ARRAY[1,2] || '3';
-- NULL resolution depends on the NULL operand's static type
SELECT ARRAY['a'] || NULL, NULL || ARRAY['a'];
SELECT ARRAY[1] || NULL::int, ARRAY[1] || NULL::int[];
SELECT ARRAY[1] || NULL::text;
-- SIMILAR TO (SQL regular expressions)
SELECT 'abc' SIMILAR TO 'a%', 'abc' SIMILAR TO 'a_c', 'abc' SIMILAR TO '(a|b)%', 'abc' SIMILAR TO 'xyz';
SELECT 'hello' NOT SIMILAR TO 'h%', 'a.c' SIMILAR TO 'a.c', 'axc' SIMILAR TO 'a.c';
SELECT 'abc123' SIMILAR TO '[a-z]+[0-9]+', 'foobar' SIMILAR TO '%(bar|baz)';
-- LIKE ANY/ALL and unknown-literal arrays in ANY/ALL
SELECT 'foo' LIKE ANY(ARRAY['f%','b%']), 'foo' LIKE ALL(ARRAY['f%','%o']), 'FOO' ILIKE ANY(ARRAY['f%']);
SELECT 'foo' NOT LIKE ANY(ARRAY['f%','b%']), 'foo' NOT LIKE ALL(ARRAY['x%','y%']);
SELECT 5 = ANY('{2,4,5}'), 3 = ANY('{2,4,5}'), 5 > ALL('{1,2,3}'), 'foo' LIKE ANY('{f%,b%}');
