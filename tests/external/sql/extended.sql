-- Extended query protocol through psql's \bind (Parse/Bind/Execute over
-- libpq) and \parse/\bind_named where available.
CREATE TABLE t_ext (id int, v text);

INSERT INTO t_ext VALUES ($1, $2) \bind 1 alpha \g
INSERT INTO t_ext VALUES ($1, $2) \bind 2 beta \g
SELECT $1::int + $2::int AS sum \bind 20 22 \g
SELECT * FROM t_ext WHERE id = $1 \bind 2 \g
SELECT v FROM t_ext WHERE id + 1 = '3' ORDER BY v \bind \g
SELECT count(*) FROM t_ext \bind \g

-- named prepared statement, reused with different parameters
SELECT v FROM t_ext WHERE id = $1 \parse find_by_id
\bind_named find_by_id 1 \g
\bind_named find_by_id 2 \g
\close_prepared find_by_id

-- extended-protocol errors recover after Sync
SELECT * FROM t_missing WHERE id = $1 \bind 1 \g
SELECT 'recovered';

DROP TABLE t_ext;
