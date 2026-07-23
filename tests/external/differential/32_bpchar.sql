-- char(n) semantics: the blank padding is part of the value (output functions
-- emit it) but is semantically insignificant (comparisons, casts to other
-- string types, and text-taking functions all strip it). Each behavior below
-- was divergent before the bpchar datum carried its own identity.
CREATE TABLE bp (c char(5), v varchar(5));
INSERT INTO bp VALUES ('hi', 'hi'), ('exact', 'exact'), ('', '');

-- Output keeps the padding; length/equality/concat strip it.
SELECT c FROM bp ORDER BY c;
SELECT '[' || c || ']', length(c), octet_length(c), bit_length(c) FROM bp ORDER BY c;
SELECT c = 'hi', c = 'hi   ', 'hi   '::text = c, c = v FROM bp ORDER BY c;

-- Casts strip; the cast's own typmod re-pads.
SELECT 'x'::char(3), length('x'::char(3)), 'x'::char(3) = 'x';
SELECT c::text, length(c::text), c::varchar(10), length(c::varchar(10)) FROM bp WHERE c = 'hi';
SELECT '12'::char(4)::int, 't '::char(3)::boolean;
SELECT 'toolong!'::char(5), 'ok   '::char(2), 'a'::char(2) = 'a'::char(3);

-- LIKE and regex see the raw padded value; text functions see it stripped.
SELECT c LIKE 'hi', c LIKE 'hi%', c LIKE 'hi   ', c SIMILAR TO 'hi', c SIMILAR TO 'hi +' FROM bp WHERE c = 'hi';
SELECT c ~ 'hi$', c ~ 'hi *$' FROM bp WHERE c = 'hi';
SELECT upper(c), substr(c, 1, 3), position('i' in c), replace(c, 'i', 'o'), reverse(c) FROM bp WHERE c = 'hi';
SELECT lpad(c, 7, '.'), rpad(c, 7, '.'), left(c, 3), split_part('a b'::char(5), ' ', 2), strpos(c, 'i') FROM bp WHERE c = 'hi';
SELECT trim(c), quote_literal(c), quote_ident(c), initcap(c), md5(c) = md5('hi') FROM bp WHERE c = 'hi';

-- Variadic-any functions use the output function (padded); text-taking
-- aggregates strip.
SELECT format('%s!', c), format('%L', c), concat(c, '!') FROM bp WHERE c = 'hi';
SELECT string_agg(c, ','), string_agg(v, ',') FROM bp WHERE c = 'hi';

-- Value passthrough keeps the padding even where the result typmod is -1.
SELECT max(c), min(c), count(DISTINCT c) FROM bp;
SELECT coalesce(c, 'z'), nullif(c, 'hi') IS NULL, greatest(c, 'a'), CASE c WHEN 'hi' THEN 'y' ELSE 'n' END FROM bp WHERE c = 'hi';
SELECT CASE WHEN true THEN c END, (SELECT c FROM bp WHERE c = 'hi') FROM bp WHERE c = 'hi';
SELECT c FROM (SELECT c FROM bp) s ORDER BY c;

-- Output functions beyond the wire: json, arrays, records.
SELECT to_json(c), to_jsonb(c), ARRAY[c], row(c)::text, json_build_object('k', c) FROM bp WHERE c = 'hi';

-- Membership and grouping strip; the unknown literal adopts bpchar-ness.
SELECT c IN ('hi'), c IN ('hi   '), c BETWEEN 'h' AND 'hz' FROM bp WHERE c = 'hi';
SELECT DISTINCT x FROM (SELECT 'a'::char(2) AS x UNION ALL SELECT 'a'::char(3)) s;
SELECT count(DISTINCT c) FROM (VALUES ('a'::char(2)), ('a'::char(3))) t(c);
SELECT c, count(*) FROM bp GROUP BY c ORDER BY c;

-- No numeric aggregates over character.
SELECT sum(c) FROM bp;

-- pg_typeof reports the static type; comparisons across string types work.
SELECT pg_typeof('x'::char(3)), pg_typeof(c) FROM bp WHERE c = 'hi';
SELECT c < 'hi!', c > 'h', c <= v, c >= v FROM bp WHERE c = 'hi';

-- Excess trailing spaces truncate silently on the column write path.
INSERT INTO bp(v) VALUES ('abcde   ');
INSERT INTO bp(c) VALUES ('abcde   ');
SELECT '[' || v || ']' FROM bp WHERE v = 'abcde';
SELECT c, octet_length(c) FROM bp WHERE c = 'abcde';
INSERT INTO bp(v) VALUES ('abcdeX');

-- RETURNING sees the bpchar value.
INSERT INTO bp(c) VALUES ('r') RETURNING '[' || c || ']', length(c), c;
DELETE FROM bp WHERE c = 'r' OR c = 'abcde' OR v = 'abcde';

-- ORDER BY and window passthrough.
SELECT c, row_number() OVER (ORDER BY c) FROM bp ORDER BY c;
SELECT lag(c) OVER (ORDER BY c) FROM bp ORDER BY c;

DROP TABLE bp;
