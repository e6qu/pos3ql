-- A derived table's parameters take the types its own expressions need.
PREPARE derived_sum AS SELECT * FROM (SELECT 1 + $1) s;
EXECUTE derived_sum(4);
PREPARE lateral_concat AS SELECT * FROM (SELECT 1) a, LATERAL (SELECT $1 || 'x') b;
EXECUTE lateral_concat('y');
PREPARE nested_derived AS SELECT * FROM (SELECT * FROM (SELECT $1 * 2.5 AS v) i) o;
EXECUTE nested_derived(2);
PREPARE derived_cast AS SELECT s.x FROM (SELECT $1::int + 1 AS x) s;
EXECUTE derived_cast(1);
SELECT name, parameter_types FROM pg_prepared_statements ORDER BY name;
DEALLOCATE ALL;
