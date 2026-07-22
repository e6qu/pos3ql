-- PostgreSQL has no equality or ordering for `json`: two documents differing
-- only in whitespace or key order are the same value but not the same text, so
-- it declines rather than answer by a rule it does not hold to, and offers
-- canonicalized `jsonb` instead. DISTINCT, DISTINCT ON, GROUP BY and ORDER BY
-- sort and deduplicate by the projected encoding and so never consult the `=`
-- operator, which is why each is checked where its keys are known.

SELECT DISTINCT x FROM (VALUES ('{}'::json)) v(x);
SELECT DISTINCT ON (x) x FROM (VALUES ('{}'::json)) v(x);
SELECT x FROM (VALUES ('{}'::json)) v(x) GROUP BY x;
SELECT x FROM (VALUES ('{}'::json)) v(x) ORDER BY x;

-- jsonb is canonicalized and does compare
SELECT DISTINCT x FROM (VALUES ('{}'::jsonb)) v(x);
SELECT x FROM (VALUES ('{}'::jsonb)) v(x) GROUP BY x;
SELECT x FROM (VALUES ('{}'::jsonb)) v(x) ORDER BY x;

-- a json column is fine as long as it is not a key
SELECT x FROM (VALUES ('{}'::json)) v(x);
SELECT x::text FROM (VALUES ('{}'::json)) v(x) ORDER BY 1;
SELECT DISTINCT a FROM (VALUES (1, '{}'::json)) v(a, b);
SELECT a FROM (VALUES (1, '{}'::json)) v(a, b) GROUP BY a;
SELECT a FROM (VALUES (1, '{}'::json)) v(a, b) ORDER BY a;

-- and the ordinary key types are unaffected
SELECT DISTINCT x FROM (VALUES (1), (1), (2)) v(x) ORDER BY x;
SELECT x FROM (VALUES (1), (2)) v(x) ORDER BY x;
SELECT x FROM (VALUES (1), (1)) v(x) GROUP BY x;
SELECT DISTINCT x FROM (VALUES ('a'), ('a')) v(x);
