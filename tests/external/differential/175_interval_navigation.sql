-- PostgreSQL 18.6 is the oracle for immutable range and network navigation.
\pset null 'NULL'

CREATE TABLE interval_navigation (
    id integer PRIMARY KEY,
    int_span int4range,
    big_span int8range,
    numeric_span numrange,
    date_span daterange,
    timestamp_span tsrange,
    timestamptz_span tstzrange,
    spans int4multirange,
    gist_address inet,
    spgist_span int4range,
    spgist_address inet,
    payload text
);
CREATE INDEX interval_navigation_int ON interval_navigation USING gist(int_span);
CREATE INDEX interval_navigation_big ON interval_navigation USING gist(big_span);
CREATE INDEX interval_navigation_numeric ON interval_navigation USING gist(numeric_span);
CREATE INDEX interval_navigation_date ON interval_navigation USING gist(date_span);
CREATE INDEX interval_navigation_timestamp ON interval_navigation USING gist(timestamp_span);
CREATE INDEX interval_navigation_timestamptz ON interval_navigation USING gist(timestamptz_span);
CREATE INDEX interval_navigation_multi ON interval_navigation USING gist(spans);
CREATE INDEX interval_navigation_gist_address
    ON interval_navigation USING gist(gist_address inet_ops);
CREATE INDEX interval_navigation_spgist_span
    ON interval_navigation USING spgist(spgist_span);
CREATE INDEX interval_navigation_spgist_address
    ON interval_navigation USING spgist(spgist_address inet_ops);

INSERT INTO interval_navigation VALUES
 (1,'[1,5)','[10000000000,10000000005)','[-1.25,2.5)',
    '[2026-01-01,2026-01-05)','[2026-01-01,2026-01-05)',
    '["2026-01-01 00:00:00+00","2026-01-05 00:00:00+00")',
    '{[1,3),[7,9)}','10.1.2.3/24','[10,15)','11.1.2.3/24','finite'),
 (2,'[20,30)','[20000000000,20000000005)','[100.01,100.02)',
    '[2030-01-01,2030-01-05)','[2030-01-01,2030-01-05)',
    '["2030-01-01 00:00:00+00","2030-01-05 00:00:00+00")',
    '{[20,30)}','192.0.2.1/32','[100,110)','198.51.100.1/32','far'),
 (3,'empty','empty','empty','empty','empty','empty','{}',
    '2001:db8::1/64','empty','2001:db8:1::1/64','empty'),
 (4,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,'null'),
 (5,'(,)','(,)','(,)','(,)','(,)','(,)','{(,)}',
    '0.0.0.0/0','(,)','::/0','unbounded');
ANALYZE interval_navigation;

-- Unknown literals must resolve from the indexed operand, not become text.
SELECT id FROM interval_navigation WHERE gist_address <<= '10.1.2.255/32' ORDER BY id;
SELECT id FROM interval_navigation WHERE spgist_address >>= '11.1.2.0/25' ORDER BY id;
SELECT id FROM interval_navigation WHERE int_span && '[4,21)' ORDER BY id;
SELECT id FROM interval_navigation WHERE spgist_span && '[14,101)' ORDER BY id;

SELECT id FROM interval_navigation WHERE int_span @> 4 ORDER BY id;
SELECT id FROM interval_navigation WHERE big_span @> 10000000002::bigint ORDER BY id;
SELECT id FROM interval_navigation WHERE numeric_span @> 0.001::numeric ORDER BY id;
SELECT id FROM interval_navigation WHERE date_span @> date '2026-01-03' ORDER BY id;
SELECT id FROM interval_navigation
 WHERE timestamp_span @> timestamp '2026-01-03 12:00:00' ORDER BY id;
SELECT id FROM interval_navigation
 WHERE timestamptz_span @> timestamptz '2026-01-03 12:00:00+00' ORDER BY id;
SELECT id FROM interval_navigation WHERE spans && '{[8,21)}' ORDER BY id;
SELECT id FROM interval_navigation WHERE int_span = 'empty'::int4range ORDER BY id;
SELECT id FROM interval_navigation WHERE spans <@ '{[1,10)}'::int4multirange ORDER BY id;
SELECT id FROM interval_navigation WHERE int_span << '[20,30)'::int4range ORDER BY id;
SELECT id FROM interval_navigation WHERE int_span -|- '[5,20)'::int4range ORDER BY id;
SELECT id FROM interval_navigation WHERE gist_address && '10.1.2.128/25' ORDER BY id;
SELECT id FROM interval_navigation WHERE spgist_address << '2001:db8:1::/48' ORDER BY id;

PREPARE interval_network_probe(inet) AS
 SELECT id FROM interval_navigation WHERE gist_address <<= $1 ORDER BY id;
EXECUTE interval_network_probe('192.0.2.1/32');
DEALLOCATE interval_network_probe;
PREPARE interval_range_probe(int4range) AS
 SELECT id FROM interval_navigation WHERE spgist_span && $1 ORDER BY id;
EXECUTE interval_range_probe('[105,106)');
DEALLOCATE interval_range_probe;

BEGIN;
SAVEPOINT before_interval_mutation;
UPDATE interval_navigation
 SET int_span='[4,5)', spans='{[8,9)}', gist_address='10.1.2.200/32',
     spgist_span='[14,15)', spgist_address='11.1.2.200/32', payload='pending'
 WHERE id=2;
SELECT id,payload FROM interval_navigation WHERE int_span @> 4 ORDER BY id;
SELECT id,payload FROM interval_navigation WHERE gist_address <<= '10.1.2.0/24' ORDER BY id;
ROLLBACK TO before_interval_mutation;
SELECT id,payload FROM interval_navigation WHERE int_span @> 4 ORDER BY id;
COMMIT;

UPDATE interval_navigation
 SET int_span='[4,5)', spans='{[8,9)}', gist_address='10.1.2.200/32',
     spgist_span='[14,15)', spgist_address='11.1.2.200/32', payload='committed'
 WHERE id=2;
DELETE FROM interval_navigation WHERE id=1;
REINDEX TABLE interval_navigation;
SELECT id,payload FROM interval_navigation WHERE int_span @> 4 ORDER BY id;
SELECT id,payload FROM interval_navigation WHERE spans && '{[8,9)}'::int4multirange ORDER BY id;
SELECT id,payload FROM interval_navigation WHERE gist_address <<= '10.1.2.0/24' ORDER BY id;
SELECT id,payload FROM interval_navigation WHERE spgist_span && '[14,15)' ORDER BY id;
SELECT id,payload FROM interval_navigation WHERE spgist_address <<= '11.1.2.0/24' ORDER BY id;

DROP TABLE interval_navigation;
