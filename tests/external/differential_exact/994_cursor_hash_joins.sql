-- Hash joins read through a cursor, a row at a time: inner rows are built
-- into the table first, or after the first outer row; with little work_mem
-- the table splits into batches, and grows them as it fills, whose outer
-- rows are saved and probed batch by batch; a skewed outer key keeps its
-- most common values' inner rows in skew buckets. Each join type returns
-- its rows in PostgreSQL's order, its unmatched inner rows after each
-- batch's outer rows. A scrollable cursor reads the join again.
CREATE TABLE ho (id int, k int, pad text);
CREATE TABLE hi (k int, w text);
INSERT INTO ho SELECT g, CASE WHEN g % 3 = 0 THEN 7 ELSE g % 900 END, repeat('o', 40) FROM generate_series(1, 3000) g;
INSERT INTO hi SELECT g % 1200, 'w' || g || repeat('i', 60) FROM generate_series(1, 2400) g;
INSERT INTO hi VALUES (NULL, 'null key');
ANALYZE ho, hi;
SET work_mem = '64kB';
EXPLAIN (COSTS OFF) SELECT ho.id, hi.w FROM ho JOIN hi ON hi.k = ho.k;
BEGIN;
DECLARE c CURSOR FOR SELECT ho.id, left(hi.w, 8) FROM ho JOIN hi ON hi.k = ho.k;
FETCH 7 FROM c;
FETCH 5 FROM c;
MOVE 4000 FROM c;
FETCH 6 FROM c;
MOVE ALL FROM c;
COMMIT;
BEGIN;
DECLARE c CURSOR FOR SELECT ho.id, left(hi.w, 8) FROM ho LEFT JOIN hi ON hi.k = ho.k WHERE ho.id % 7 = 0;
FETCH 9 FROM c;
MOVE 1000 FROM c;
FETCH 9 FROM c;
COMMIT;
EXPLAIN (COSTS OFF) SELECT ho.id, hi.w FROM ho FULL JOIN hi ON hi.k = ho.k + 1000;
BEGIN;
DECLARE c CURSOR FOR SELECT ho.id, left(hi.w, 8) FROM ho FULL JOIN hi ON hi.k = ho.k + 1000;
FETCH 5 FROM c;
MOVE 3000 FROM c;
FETCH 40 FROM c;
MOVE 1000 FROM c;
FETCH 40 FROM c;
COMMIT;
BEGIN;
DECLARE c CURSOR FOR SELECT left(hi.w, 8) FROM hi WHERE NOT EXISTS (SELECT FROM ho WHERE ho.k + 300 = hi.k);
FETCH 20 FROM c;
MOVE 200 FROM c;
FETCH 20 FROM c;
COMMIT;
BEGIN;
DECLARE c CURSOR FOR SELECT left(hi.w, 8) FROM hi WHERE EXISTS (SELECT FROM ho WHERE ho.k = hi.k);
FETCH 20 FROM c;
MOVE 1000 FROM c;
FETCH 20 FROM c;
COMMIT;
BEGIN;
DECLARE c CURSOR FOR SELECT ho.id FROM ho WHERE NOT EXISTS (SELECT FROM hi WHERE hi.k = ho.k + 1000);
FETCH 20 FROM c;
MOVE 500 FROM c;
FETCH 20 FROM c;
COMMIT;
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT ho.id, left(hi.w, 8) FROM ho JOIN hi ON hi.k = ho.k;
FETCH 6 FROM c;
FETCH BACKWARD 3 FROM c;
FETCH ABSOLUTE 2 FROM c;
FETCH ABSOLUTE 4100 FROM c;
FETCH BACKWARD 4 FROM c;
FETCH LAST FROM c;
COMMIT;
RESET work_mem;
DROP TABLE ho, hi;
