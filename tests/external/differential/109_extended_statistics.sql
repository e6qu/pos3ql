DROP SCHEMA IF EXISTS extended_statistics_archive CASCADE;
DROP TABLE IF EXISTS extended_statistics_source CASCADE;

CREATE TABLE extended_statistics_source (a integer, b integer, label text);
INSERT INTO extended_statistics_source VALUES
    (1, 1, 'One'), (1, 1, 'ONE'), (2, 2, 'Two'), (NULL, 3, NULL);
CREATE STATISTICS extended_statistics_ab (ndistinct, dependencies, mcv)
    ON a, b FROM extended_statistics_source;
CREATE STATISTICS extended_statistics_label
    ON (lower(label)) FROM extended_statistics_source;

SELECT stxname, stxstattarget, stxkeys,
       pg_get_statisticsobjdef_columns(oid)
FROM pg_statistic_ext
WHERE stxname LIKE 'extended_statistics_%'
ORDER BY stxname;

ANALYZE extended_statistics_source;
SELECT e.stxname,
       d.stxdndistinct IS NOT NULL,
       d.stxddependencies IS NOT NULL,
       d.stxdmcv IS NOT NULL,
       d.stxdexpr IS NOT NULL
FROM pg_statistic_ext e
JOIN pg_statistic_ext_data d ON d.stxoid = e.oid
WHERE e.stxname LIKE 'extended_statistics_%'
ORDER BY e.stxname;

ALTER STATISTICS extended_statistics_ab SET STATISTICS 12;
ALTER STATISTICS extended_statistics_ab RENAME TO extended_statistics_ab_renamed;
CREATE SCHEMA extended_statistics_archive;
ALTER STATISTICS extended_statistics_ab_renamed SET SCHEMA extended_statistics_archive;
CREATE STATISTICS extended_statistics_bl ON b, label FROM extended_statistics_source;
ALTER STATISTICS extended_statistics_bl SET SCHEMA extended_statistics_archive;
SELECT n.nspname, e.stxname, e.stxstattarget
FROM pg_statistic_ext e
JOIN pg_namespace n ON n.oid = e.stxnamespace
WHERE e.stxname = 'extended_statistics_ab_renamed';

BEGIN;
ALTER TABLE extended_statistics_source RENAME COLUMN label TO renamed_label;
SELECT pg_get_statisticsobjdef_columns(oid)
FROM pg_statistic_ext WHERE stxname = 'extended_statistics_label';
ROLLBACK;
SELECT pg_get_statisticsobjdef_columns(oid)
FROM pg_statistic_ext WHERE stxname = 'extended_statistics_label';

ALTER TABLE extended_statistics_source DROP COLUMN a;
SELECT count(*) FROM pg_statistic_ext
WHERE stxname = 'extended_statistics_ab_renamed';
DROP SCHEMA extended_statistics_archive RESTRICT;
DROP SCHEMA extended_statistics_archive CASCADE;
SELECT count(*) FROM pg_statistic_ext WHERE stxname = 'extended_statistics_bl';
SELECT count(*) FROM extended_statistics_source;
DROP TABLE extended_statistics_source;
