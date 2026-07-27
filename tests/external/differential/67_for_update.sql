-- Row-locking clauses: FOR { UPDATE | NO KEY UPDATE | SHARE | KEY SHARE }
-- [OF t, …] [NOWAIT | SKIP LOCKED], matching PostgreSQL 18. In a single
-- session a locking clause returns the same rows as the plain query; the
-- corpus pins that, the analysis-time restrictions, and OF resolution. (The
-- cross-transaction blocking/NOWAIT/SKIP-LOCKED behavior is not exercised
-- here — it needs two concurrent sessions the differential harness does not
-- drive; see BUGS.md.)
--
-- Distinctive names + drop up front (the differential corpora share a database).
DROP TABLE IF EXISTS fu_t;

CREATE TABLE fu_t (id int, v int);
INSERT INTO fu_t VALUES (1, 10), (2, 20), (3, 30);

-- Every lock strength returns the query's rows unchanged.
SELECT id FROM fu_t ORDER BY id FOR UPDATE;
SELECT id FROM fu_t ORDER BY id FOR NO KEY UPDATE;
SELECT id FROM fu_t ORDER BY id FOR SHARE;
SELECT id FROM fu_t ORDER BY id FOR KEY SHARE;

-- OF names the base table or its alias; NOWAIT / SKIP LOCKED parse; a clause
-- may follow LIMIT; multiple clauses may combine.
SELECT id FROM fu_t ORDER BY id FOR UPDATE OF fu_t;
SELECT id FROM fu_t t1 ORDER BY id FOR UPDATE OF t1;
SELECT id FROM fu_t ORDER BY id FOR UPDATE NOWAIT;
SELECT id FROM fu_t ORDER BY id FOR UPDATE SKIP LOCKED;
SELECT id FROM fu_t ORDER BY id LIMIT 1 FOR UPDATE;
SELECT id FROM fu_t ORDER BY id FOR UPDATE FOR SHARE OF fu_t;

-- A FROM-less SELECT may carry FOR UPDATE (it locks nothing).
SELECT 1 FOR UPDATE;

-- Analysis-time restrictions (0A000), reported with the clause's own keyword:
SELECT count(*) FROM fu_t FOR UPDATE;
SELECT id FROM fu_t GROUP BY id FOR UPDATE;
SELECT id FROM fu_t GROUP BY id HAVING count(*) > 0 FOR SHARE;
SELECT DISTINCT id FROM fu_t FOR UPDATE;
SELECT id, row_number() OVER (ORDER BY id) FROM fu_t FOR UPDATE;
SELECT id FROM fu_t UNION SELECT v FROM fu_t FOR UPDATE;

-- OF must name a relation of the FROM clause (42P01); an aliased table is
-- reachable only by its alias, not its original name.
SELECT id FROM fu_t FOR UPDATE OF nosuch;
SELECT id FROM fu_t x FOR UPDATE OF fu_t;

DROP TABLE fu_t;
