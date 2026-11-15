-- A server that refuses the connection fails as connect_pg_server reports
-- it: SQLSTATE 08001 with libpq's message as the detail.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER cr_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER cr_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE cr_t (id int) SERVER cr_s;
SELECT * FROM cr_t;
CREATE SERVER cr_a FOREIGN DATA WRAPPER postgres_fdw OPTIONS (hostaddr '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER cr_a OPTIONS (user 'nobody');
CREATE FOREIGN TABLE cr_u (id int) SERVER cr_a;
SELECT count(*) FROM cr_u;
DROP SERVER cr_s, cr_a CASCADE;
