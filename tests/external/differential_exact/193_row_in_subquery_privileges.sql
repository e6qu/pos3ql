-- A row IN or quantified subquery checks EXECUTE on each column's equality
-- function once a built-in function's privileges change.
CREATE ROLE ri_user;
REVOKE EXECUTE ON FUNCTION int4eq(integer, integer) FROM PUBLIC;
SELECT (1,2) IN (SELECT 1, 2);
SELECT (1,'a') IN (SELECT 1, 'a' UNION ALL SELECT 3, 'b');
SET ROLE ri_user;
SELECT (1,2) IN (SELECT 1, 2);
SELECT ('a','b') IN (SELECT 'a', 'b');
SELECT ('a',2) IN (SELECT 'a', 2);
RESET ROLE;
GRANT EXECUTE ON FUNCTION int4eq(integer, integer) TO PUBLIC;
SELECT (1,2) IN (SELECT 1, 2);
DROP ROLE ri_user;
