-- Window functions: ranking, value, offset, and aggregate calls over
-- PARTITION BY / ORDER BY and ROWS / RANGE / GROUPS frames with EXCLUDE,
-- plus PostgreSQL's parse-analysis rejections of DISTINCT / aggregate
-- ORDER BY / non-aggregate FILTER on window calls (0A000).
-- Window ORDER BY lists are total (ties broken by `name`) wherever the
-- result depends on the order among peers; peer-tied keys remain only
-- where PostgreSQL's answer is itself deterministic (rank families and
-- RANGE/GROUPS frames).

CREATE TABLE wf (grp int, val int, name text);
INSERT INTO wf VALUES
  (1,10,'a'),(1,20,'b'),(1,20,'c'),(1,30,'d'),
  (2,5,'e'),(2,15,'f'),(2,15,'g'),
  (NULL,7,'h'),(NULL,9,'i'),(NULL,9,'j');

-- rankings: peers share rank/dense_rank/percent_rank/cume_dist; row_number
-- enumerates the sorted partition (tie-broken for determinism)
SELECT grp, val, row_number() OVER (PARTITION BY grp ORDER BY val, name),
  rank() OVER (PARTITION BY grp ORDER BY val),
  dense_rank() OVER (PARTITION BY grp ORDER BY val),
  percent_rank() OVER (PARTITION BY grp ORDER BY val),
  cume_dist() OVER (PARTITION BY grp ORDER BY val)
FROM wf ORDER BY grp, val, name;

-- no ORDER BY: the whole partition is one peer group
SELECT grp, val, rank() OVER (PARTITION BY grp), dense_rank() OVER (PARTITION BY grp),
  percent_rank() OVER (PARTITION BY grp), cume_dist() OVER (PARTITION BY grp)
FROM wf ORDER BY grp, val, name;

-- no PARTITION BY: one window over all rows
SELECT val, row_number() OVER (ORDER BY val, name), rank() OVER (ORDER BY val)
FROM wf ORDER BY val, name;

-- empty OVER ()
SELECT val, row_number() OVER (), count(*) OVER () FROM wf ORDER BY val, name;

-- descending order and NULLS placement
SELECT grp, val, row_number() OVER (PARTITION BY grp ORDER BY val DESC NULLS FIRST, name)
FROM wf ORDER BY grp, val DESC NULLS FIRST, name;
SELECT grp, val, row_number() OVER (PARTITION BY grp ORDER BY val DESC NULLS LAST, name)
FROM wf ORDER BY grp, val DESC NULLS LAST, name;

-- aggregates: running vs whole-partition
SELECT grp, val, sum(val) OVER (PARTITION BY grp ORDER BY val),
  sum(val) OVER (PARTITION BY grp),
  count(*) OVER (PARTITION BY grp ORDER BY val),
  avg(val) OVER (PARTITION BY grp ORDER BY val),
  min(val) OVER (PARTITION BY grp ORDER BY val),
  max(val) OVER (PARTITION BY grp ORDER BY val)
FROM wf ORDER BY grp, val, name;

-- FILTER on an aggregate window call: excluded rows fall out of the frame sum
SELECT grp, val, sum(val) FILTER (WHERE val > 10) OVER (PARTITION BY grp ORDER BY val)
FROM wf ORDER BY grp, val, name;

-- lag / lead with offsets and defaults
SELECT val, lag(val) OVER (ORDER BY val, name), lag(val, 2, -1) OVER (ORDER BY val, name),
  lead(val) OVER (ORDER BY val, name), lead(val, 2, -1) OVER (ORDER BY val, name)
FROM wf ORDER BY val, name;

-- first_value / last_value / nth_value / ntile
SELECT grp, val, first_value(val) OVER (PARTITION BY grp ORDER BY val, name),
  last_value(val) OVER (PARTITION BY grp ORDER BY val, name),
  nth_value(val, 2) OVER (PARTITION BY grp ORDER BY val, name),
  ntile(2) OVER (PARTITION BY grp ORDER BY val, name)
FROM wf ORDER BY grp, val, name;

-- ROWS frame: running and sliding windows over a total order
SELECT grp, val, sum(val) OVER (PARTITION BY grp ORDER BY val, name ROWS BETWEEN 1 PRECEDING AND CURRENT ROW),
  sum(val) OVER (PARTITION BY grp ORDER BY val, name ROWS BETWEEN 2 PRECEDING AND 1 FOLLOWING),
  sum(val) OVER (PARTITION BY grp ORDER BY val, name ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)
FROM wf ORDER BY grp, val, name;

-- RANGE frame with an offset over the ordered key (peer groups stay whole)
SELECT grp, val, count(*) OVER (PARTITION BY grp ORDER BY val RANGE BETWEEN 10 PRECEDING AND CURRENT ROW),
  sum(val) OVER (PARTITION BY grp ORDER BY val RANGE BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING)
FROM wf ORDER BY grp, val, name;

-- GROUPS frame with EXCLUDE variants over peer-tied keys
SELECT grp, val, sum(val) OVER (PARTITION BY grp ORDER BY val GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW),
  sum(val) OVER (PARTITION BY grp ORDER BY val GROUPS BETWEEN CURRENT ROW AND CURRENT ROW EXCLUDE TIES),
  sum(val) OVER (PARTITION BY grp ORDER BY val GROUPS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE GROUP),
  sum(val) OVER (PARTITION BY grp ORDER BY val GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW EXCLUDE NO OTHERS)
FROM wf ORDER BY grp, val, name;

-- EXCLUDE on a plain ROWS frame over a total order
SELECT grp, val, sum(val) OVER (PARTITION BY grp ORDER BY val, name ROWS BETWEEN 2 PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW)
FROM wf ORDER BY grp, val, name;

-- named WINDOW clause, reused and refined with an added ORDER BY
SELECT grp, val, sum(val) OVER w, sum(val) OVER (w ORDER BY val, name), count(*) OVER w
FROM wf WINDOW w AS (PARTITION BY grp) ORDER BY grp, val, name;

-- several window calls in one projection, mixing frame kinds
SELECT grp, val,
  row_number() OVER (PARTITION BY grp ORDER BY val, name),
  sum(val) OVER (PARTITION BY grp ORDER BY val),
  lag(val) OVER (PARTITION BY grp ORDER BY val, name),
  first_value(val) OVER (PARTITION BY grp ORDER BY val, name)
FROM wf ORDER BY grp, val, name;

-- window functions compose with outer ORDER BY and DISTINCT
SELECT DISTINCT grp, count(*) OVER (PARTITION BY grp) AS cnt FROM wf ORDER BY grp;
SELECT grp, val, rank() OVER (PARTITION BY grp ORDER BY val) AS r FROM wf ORDER BY r, val, name;

-- PostgreSQL rejects DISTINCT and aggregate ORDER BY in a window call, but
-- accepts FILTER on an aggregate window function.
SELECT count(DISTINCT val) OVER (PARTITION BY grp) FROM wf;
SELECT sum(DISTINCT val) OVER (ORDER BY val) FROM wf;
SELECT array_agg(val ORDER BY val) OVER (PARTITION BY grp) FROM wf;
SELECT sum(val) FILTER (WHERE val > 1) OVER (PARTITION BY grp) FROM wf;
SELECT row_number() FILTER (WHERE true) OVER (ORDER BY val) FROM wf;
SELECT lag(val) FILTER (WHERE true) OVER (ORDER BY val) FROM wf;

DROP TABLE wf;
