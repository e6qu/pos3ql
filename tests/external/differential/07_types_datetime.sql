-- Dates, timestamps, uuid, bytea.
SET timezone TO 'UTC';
CREATE TABLE ev7 (id int, d date, t timestamptz, u uuid, raw bytea);
INSERT INTO ev7 VALUES
  (1, '2024-02-29', '2024-02-29 12:30:45.5+00', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '\xdeadbeef'),
  (2, '2000-01-01', '2024-03-01T00:00:00+02',   'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A99', '\x00ff');
SELECT * FROM ev7 ORDER BY id;
SELECT id FROM ev7 WHERE d > '2020-01-01' ORDER BY id;
SELECT id FROM ev7 WHERE t BETWEEN '2024-02-01' AND '2024-02-29 23:59:59Z' ORDER BY id;
SELECT '2024-06-15'::date, '2024-06-15 10:00'::timestamp;
SELECT 'b0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid, '\x0102'::bytea;
SELECT max(t), min(d) FROM ev7;
SELECT id FROM ev7 WHERE u = 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11';
SELECT '2023-02-29'::date;
DROP TABLE ev7;
-- interval scaling and justification
SELECT interval '1 day' * 3, interval '1 month' * 1.5, interval '2 hours' * 2.5;
SELECT interval '1 day' / 2, interval '10 days' / 3, 2 * interval '3 hours';
SELECT justify_hours(interval '36 hours'), justify_days(interval '35 days');
SELECT justify_interval(interval '1 mon -1 hour');
-- age (symbolic calendar interval)
SELECT age(timestamp '2024-06-15', timestamp '2020-01-10');
SELECT age(timestamp '2020-01-10', timestamp '2024-06-15');
SELECT age(timestamp '2024-03-01', timestamp '2024-01-31');
SELECT age(timestamp '2024-01-01 10:00', timestamp '2023-12-15 14:30');
SELECT age(date '2000-01-01', date '1999-02-05');
-- to_timestamp(epoch): Unix seconds -> timestamptz
SELECT to_timestamp(0), to_timestamp(1700000000), to_timestamp(1700000000.5), to_timestamp(-100000);
