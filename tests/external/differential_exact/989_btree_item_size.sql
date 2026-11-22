-- A btree entry longer than BTMaxItemSize (2,704 bytes with 8 kB pages)
-- after index_form_tuple compresses its values fails as
-- _bt_check_third_page does, naming the heap tuple the row takes. A
-- compressible value is indexed compressed; a short one takes a one-byte
-- header; a NULL adds the null bitmap; INCLUDE columns count; a name key
-- is stored as a cstring.
CREATE TABLE bt (id int, t text);
CREATE INDEX bt_t ON bt (t);
INSERT INTO bt VALUES (1, 'x');
INSERT INTO bt VALUES (3, (SELECT string_agg(md5(i::text), '') FROM generate_series(1, 84) i));
INSERT INTO bt VALUES (4, repeat('a', 100000));
INSERT INTO bt VALUES (2, (SELECT string_agg(md5(i::text), '') FROM generate_series(1, 90) i));
SELECT id, length(t) FROM bt ORDER BY id;
DROP TABLE bt;
CREATE TABLE bu (id int, t text UNIQUE);
INSERT INTO bu VALUES (1, (SELECT string_agg(md5(i::text), '') FROM generate_series(1, 85) i));
DROP TABLE bu;
CREATE TABLE bp (t text PRIMARY KEY, n int);
INSERT INTO bp VALUES ((SELECT string_agg(md5(i::text), '') FROM generate_series(1, 84) i) || 'abcdefgh', 1);
DROP TABLE bp;
CREATE TABLE bn (a text, b int, c text);
CREATE INDEX bn_ab ON bn (a, b) INCLUDE (c);
INSERT INTO bn VALUES ((SELECT string_agg(md5(i::text), '') FROM generate_series(1, 84) i), NULL, NULL);
INSERT INTO bn VALUES (repeat('z', 2000), 1, (SELECT string_agg(md5(i::text), '') FROM generate_series(1, 30) i));
DROP TABLE bn;
CREATE TABLE bc (id int, t text);
INSERT INTO bc SELECT i, CASE WHEN i = 37 THEN (SELECT string_agg(md5(j::text), '') FROM generate_series(1, 100) j) ELSE repeat('r', i * 20) END FROM generate_series(1, 60) i;
CREATE INDEX bc_t ON bc (t);
CREATE INDEX bc_l ON bc (lower(t));
CREATE INDEX bc_h ON bc USING hash (t);
DROP TABLE bc;
CREATE TABLE bv (v varchar(4000), n name);
CREATE INDEX bv_v ON bv (v, n);
INSERT INTO bv VALUES ((SELECT string_agg(md5(i::text), '') FROM generate_series(1, 84) i), 'xxx');
INSERT INTO bv VALUES ((SELECT string_agg(md5(i::text), '') FROM generate_series(1, 84) i), 'xxxxxx');
DROP TABLE bv;
