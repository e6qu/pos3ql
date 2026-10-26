CREATE VIEW identity_json_report AS SELECT 1 AS id, 'ok'::varchar(2) AS label;
SELECT (jsonb_populate_record(NULL::identity_json_report, '{"id":9,"label":"hi"}'::jsonb)).id;
SELECT (jsonb_populate_record(NULL::identity_json_report, '{"id":9,"label":"hi"}'::jsonb)).label;
SELECT jsonb_populate_record(NULL::identity_json_report, '{"id":9,"label":"long"}'::jsonb);
DROP VIEW identity_json_report;
