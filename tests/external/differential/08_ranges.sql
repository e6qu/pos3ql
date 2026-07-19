-- Range types: construction, canonicalization, casts, functions, operators.
SELECT int4range(1, 10), int4range(1, 10, '[]'), int4range(5, 5), int4range(NULL, 5);
SELECT numrange(1.0, 5.0), numrange(1.00, 5.0), numrange(2.5, 2.5, '[]'), numrange(NULL, NULL);
SELECT int8range(1, 100), daterange('2024-01-01', '2024-03-01');
SELECT '[1,10)'::int4range, '(1,10]'::int4range, 'empty'::int4range, '[1,)'::int4range;
SELECT '  [1 , 10 )  '::int4range, '[1,1]'::numrange;
SELECT lower(int4range(3, 9)), upper(int4range(3, 9)), lower(int4range(NULL, 9)), upper('[1,)'::int4range);
SELECT isempty(int4range(5, 5)), isempty(int4range(1, 5)), isempty('empty'::int4range);
SELECT lower_inc(int4range(1, 5)), upper_inc(int4range(1, 5)), lower_inc('empty'::int4range);
SELECT lower_inc(numrange(1, 5, '[]')), upper_inc(numrange(1, 5, '[]'));
-- containment / overlap
SELECT int4range(1, 10) @> 5, int4range(1, 10) @> 10, int4range(1, 10) @> 1;
SELECT int4range(1, 10) @> int4range(2, 5), int4range(1, 10) @> int4range(5, 15);
SELECT int4range(2, 5) <@ int4range(1, 10), int4range(1, 15) <@ int4range(1, 10);
SELECT int4range(1, 5) && int4range(4, 10), int4range(1, 5) && int4range(6, 10);
SELECT numrange(1.0, 5.0) @> 2.5, numrange(1.0, 5.0) @> 5.0, numrange(2.50, 3) @> 2.5;
SELECT daterange('2024-01-01', '2024-03-01') @> '2024-02-15'::date;
SELECT tsrange('2024-01-01', '2024-06-01') @> '2024-03-01'::timestamp;
SELECT 5 <@ int4range(1, 10), 2.5 <@ numrange(1.0, 5.0);
-- equality / ordering (bound-value based, not text)
SELECT int4range(1, 5) = int4range(1, 5), int4range(1, 5, '[]') = int4range(1, 6);
SELECT numrange(1.0, 5.0) = numrange(1.00, 5.0), numrange(1.0, 5.0) <> numrange(1.0, 5.1);
SELECT int4range(1, 5) < int4range(1, 6), int4range(1, 5) < int4range(2, 3), int4range(1, 10) > int4range(1, 5);
SELECT numrange(-5.0, -1.0) < numrange(-5.0, -0.5), numrange(1.0, 5.0) < numrange(1.0, 5.0);
-- sorting, grouping, distinct over a table (non-empty ranges)
CREATE TABLE rng (id int, r int4range);
INSERT INTO rng VALUES (1, int4range(5, 10)), (2, int4range(1, 3)), (3, int4range(1, 8)), (4, int4range(1, 3, '[]')), (5, int4range(1, 3));
SELECT id, r FROM rng ORDER BY r, id;
SELECT r FROM rng GROUP BY r ORDER BY r;
SELECT count(DISTINCT r) FROM rng;
SELECT id FROM rng WHERE r @> 2 ORDER BY id;
SELECT id FROM rng WHERE r < int4range(1, 8) ORDER BY id;
DROP TABLE rng;
-- error cases
SELECT int4range(10, 1);
SELECT '[1,2,3)'::int4range;
SELECT int4range(1, 5) = numrange(1, 5);
