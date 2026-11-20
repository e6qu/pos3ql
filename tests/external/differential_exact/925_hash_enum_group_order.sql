-- An enum column is a grouping key a HashAggregate hashes (hashenum: its
-- label's pg_enum OID), also for a DISTINCT and a materialized view
-- refresh. PostgreSQL's label OIDs come from the cluster's OID counter, so
-- the order of the groups differs between clusters and these queries sort
-- their groups; they must run, not fail for want of a hash function.
CREATE TYPE e925 AS ENUM ('red', 'green', 'blue', 'cyan', 'magenta', 'yellow', 'black', 'white');
CREATE TABLE h925 (id int PRIMARY KEY, c e925, d e925);
INSERT INTO h925
SELECT i, (enum_range(NULL::e925))[1 + i % 8], (enum_range(NULL::e925))[1 + i % 3]
FROM generate_series(1, 3000) i;
ANALYZE h925;
SELECT c, count(*) FROM h925 GROUP BY c ORDER BY c;
SELECT c, d, count(*) FROM h925 GROUP BY c, d ORDER BY c, d;
SELECT count(*) FROM (SELECT c, count(*) FROM h925 GROUP BY c) s;
SELECT count(*) FROM (SELECT DISTINCT c FROM h925) s;
SELECT DISTINCT c FROM h925 ORDER BY c;
CREATE MATERIALIZED VIEW m925 AS SELECT c, count(*) AS n FROM h925 GROUP BY c;
REFRESH MATERIALIZED VIEW m925;
SELECT * FROM m925 ORDER BY c;
