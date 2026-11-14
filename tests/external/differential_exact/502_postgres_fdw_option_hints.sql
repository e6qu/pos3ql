-- postgres_fdw's validator hints at the closest option a context accepts
-- (getClosestMatch), or that the context accepts none.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER oh_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (hots 'x');
CREATE SERVER oh_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (fetch_sise '5');
CREATE SERVER oh_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (zzzzzzzzzzzzzz '5');
CREATE SERVER oh_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (user 'x');
CREATE SERVER oh_s FOREIGN DATA WRAPPER postgres_fdw;
CREATE USER MAPPING FOR CURRENT_USER SERVER oh_s OPTIONS (usr 'x');
CREATE USER MAPPING FOR CURRENT_USER SERVER oh_s OPTIONS (host 'x');
CREATE FOREIGN TABLE oh_t (a int) SERVER oh_s OPTIONS (table_nam 'x');
CREATE FOREIGN TABLE oh_t (a int OPTIONS (colum_name 'b')) SERVER oh_s;
ALTER FOREIGN DATA WRAPPER postgres_fdw OPTIONS (ADD foo 'x');
DROP SERVER oh_s;
SELECT postgres_fdw_validator(ARRAY['fetch_sise=1'], 0);
SELECT postgres_fdw_validator(ARRAY['fetch_sise=1'], 'pg_foreign_table'::regclass);
