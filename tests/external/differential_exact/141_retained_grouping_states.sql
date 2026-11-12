-- Retained groupings whose aggregates all keep fixed-size states fold each
-- row into its group as it is read: every such aggregate and argument type,
-- NULL and empty groups, NaN and signed zero keys, and errors match
-- PostgreSQL. A routine call in the output hands each group off on its row.
CREATE TABLE state_rows(id int, grp int, small smallint, big bigint, r real,
  d double precision, span interval, cash money, day date, at timestamptz,
  u uuid, flag boolean, code "char");
INSERT INTO state_rows SELECT g, g % 5, (g * 3)::smallint, g * 1000000000::bigint,
  g / 4.0, g / 8.0, g * interval '1 hour 1 minute', (g * 1.25)::money,
  date '2024-01-01' + g, timestamptz '2024-01-01 00:00+00' + g * interval '7 hours',
  ('00000000-0000-0000-0000-' || lpad(g::text, 12, '0'))::uuid, g % 3 = 0,
  chr(97 + g % 7)::"char"
  FROM generate_series(1, 40) g;
INSERT INTO state_rows VALUES (41, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
  NULL, NULL, NULL, NULL, NULL), (42, 1, NULL, NULL, 'NaN', NULL, NULL, NULL,
  NULL, NULL, NULL, NULL, NULL), (43, NULL, NULL, NULL, NULL, '-0', NULL, NULL,
  NULL, NULL, NULL, NULL, NULL);
CREATE FUNCTION state_tag(i bigint) RETURNS text LANGUAGE sql AS $$ SELECT 'g' || i $$;
SELECT grp, state_tag(count(*)), count(small), sum(small), sum(id), sum(big),
  avg(small), avg(big), sum(r), avg(r), sum(d), avg(d)
  FROM state_rows GROUP BY grp ORDER BY grp;
SELECT grp, state_tag(grp), sum(span), avg(span), sum(cash), min(cash), max(cash),
  min(day), max(day), min(at), max(at), min(code), max(code), count(u)
  FROM state_rows GROUP BY grp ORDER BY grp;
SELECT grp, state_tag(grp), min(r), max(r), min(d), max(d), min(big), max(small),
  bool_and(flag), bool_or(flag), every(id > 2), bit_and(id), bit_or(small),
  bit_xor(big)
  FROM state_rows GROUP BY grp ORDER BY grp;
SELECT grp, state_tag(grp), var_pop(d), var_samp(r), stddev_pop(d), stddev(r),
  variance(d), corr(id, d), covar_pop(id, big), covar_samp(d, r),
  regr_slope(d, id), regr_intercept(d, id), regr_r2(d, id), regr_count(d, id),
  regr_avgx(d, id), regr_avgy(d, id), regr_sxx(d, id), regr_syy(d, id), regr_sxy(d, id)
  FROM state_rows GROUP BY grp ORDER BY grp;
SELECT state_tag(count(*)), count(d), sum(d), max(at), bool_and(flag)
  FROM state_rows WHERE id < 0;
SELECT state_tag(count(*)), sum(id), min(day) FROM state_rows;
SELECT d, state_tag(count(*)) FROM state_rows WHERE id > 30 OR id = 43
  GROUP BY d ORDER BY d;
SELECT grp, state_tag(count(*) FILTER (WHERE id > 3)), sum(id) FILTER (WHERE id % 2 = 0)
  FROM state_rows GROUP BY grp HAVING count(*) > 7 ORDER BY grp;
SELECT grp, state_tag(sum(1 / (id - 13))) FROM state_rows GROUP BY grp ORDER BY grp;
SELECT state_tag(sum(cash)::numeric::bigint), sum(x) FROM state_rows,
  (VALUES (9223372036854775807::bigint)) v(x);
SELECT k, state_tag(count(*)), sum(k) FROM generate_series(1, 2000) g,
  LATERAL (SELECT g % 7 AS k) s GROUP BY k ORDER BY k;
SELECT g % 997 AS k, state_tag(count(*)), sum(g), max(g) FROM generate_series(1, 5000) g
  GROUP BY g % 997 ORDER BY g % 997 LIMIT 3 OFFSET 400;
DROP ROUTINE state_tag(bigint) CASCADE;
