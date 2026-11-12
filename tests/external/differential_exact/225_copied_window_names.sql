-- A leading name in OVER (...) copies a window; the grammar accepts only a
-- window clause or ')' after it, so OVER (EXCLUDE CURRENT ROW) fails at CURRENT.
CREATE SCHEMA copied_window_names;
SET search_path = copied_window_names, public;
create table ex(v int); insert into ex values (1),(2);
SELECT v, sum(v) OVER (EXCLUDE CURRENT ROW) FROM ex;
SELECT v, sum(v) OVER (w) FROM ex WINDOW w AS (ORDER BY v);
SELECT v, sum(v) OVER (w ORDER BY v) FROM ex WINDOW w AS (PARTITION BY v);
SELECT v, sum(v) OVER (nosuch) FROM ex;
SELECT v, sum(v) OVER (nosuch v) FROM ex;
SELECT v, sum(v) OVER ("w" ROWS UNBOUNDED PRECEDING) FROM ex WINDOW w AS (ORDER BY v);
SELECT v, sum(v) OVER (exclude) FROM ex;
