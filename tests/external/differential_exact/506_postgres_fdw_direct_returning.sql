-- A direct foreign UPDATE or DELETE ships the columns its RETURNING list
-- reads; the write node prints that list. RETURNING old or new values
-- changes the rows one at a time.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER dr_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER dr_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE dr_t (id int, q int, note text OPTIONS (column_name 'Note'))
  SERVER dr_s OPTIONS (schema_name 'remote', table_name 't');
EXPLAIN (VERBOSE, COSTS OFF) UPDATE dr_t SET q = q + 1 WHERE id = 3 RETURNING id, q;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE dr_t SET q = q + 1 WHERE id = 3 RETURNING *;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE dr_t SET q = q + 1 WHERE id = 3 RETURNING 1;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE dr_t t SET q = t.q + 1 WHERE t.id = 3 RETURNING t.id, t.q + 1, tableoid::regclass, t;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE dr_t SET q = q + 1 WHERE id = 3 RETURNING old.q, new.q;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE dr_t SET q = q + 1 WHERE id = 3 RETURNING WITH (OLD AS o) o.*, q;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM dr_t WHERE id = 4 RETURNING note, upper(note);
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM dr_t WHERE id = 4 RETURNING dr_t;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM dr_t WHERE id = 4 AND random() < 2 RETURNING note;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE dr_t SET q = random() WHERE id = 3 RETURNING id;
DROP SERVER dr_s CASCADE;
