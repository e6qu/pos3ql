-- PostgreSQL 18.6 hashfunc.c and hashfn.c, including every byte-tail length.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/access/hash/hashfunc.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/common/hashfn.c
SELECT n,hashtext(repeat('a',n)),hashtextextended(repeat('a',n),0),
       hashtextextended(repeat('a',n),123),hashtextextended(repeat('a',n),-1),
       hashbytea(convert_to(repeat('a',n),'UTF8')),
       hashbyteaextended(convert_to(repeat('a',n),'UTF8'),123)
FROM generate_series(0,60) AS n ORDER BY n;
SELECT hashtext('order-42'),hashtextextended('order-42',0),
       hashtextextended('order-42','9223372036854775807'),
       hashtextextended('Mârza 雪',123),hashtext('x '),hashbpchar('x '),
       hashbpcharextended('x  ',123),hashname(repeat('x',64)),hashnameextended('snow',123);
SELECT hashint2(-1::smallint),hashint4(-1),hashint8(-1),hashint8('-9223372036854775808'),
       hashint2extended(-1::smallint,123),hashint4extended(-1,123),hashint8extended(-1,123),
       hashoid('4294967295'::oid),hashoidextended('4294967295'::oid,123),
       hashchar('a'),hashcharextended('a',123),hashbool(true),hashboolextended(false,123);
SELECT hashfloat4(1.5::real),hashfloat8(1.5::float8),hashfloat4('NaN'),hashfloat8('NaN'),
       hashfloat4('infinity'),hashfloat8('-infinity'),hashfloat4('-0'),hashfloat8('-0'),
       hashfloat4extended('NaN',123),hashfloat8extended('NaN',123),
       hashfloat4extended('-0',123),hashfloat8extended('-0',-1);
SELECT hashtext(NULL::text),hashtextextended(NULL::text,0),hashtextextended('x',NULL::bigint),
       pg_typeof(hashtext(NULL::text)),pg_typeof(hashtextextended(NULL::text,0));
CREATE SEQUENCE hash_calls START 10;
SELECT hashtextextended(NULL::text,nextval('hash_calls'));
SELECT last_value,is_called FROM hash_calls;
CREATE TABLE hash_inputs(value text);
INSERT INTO hash_inputs VALUES(NULL);
SELECT hashtextextended(value,nextval('hash_calls')) FROM hash_inputs;
SELECT last_value,is_called FROM hash_calls;
DROP TABLE hash_inputs;
DROP SEQUENCE hash_calls;
CREATE DOMAIN hash_label AS text;
SELECT hashtext('order-42'::hash_label),hashtextextended(NULL::hash_label,123);
DROP DOMAIN hash_label;
SELECT oid,proname,prorettype,proargtypes FROM pg_proc WHERE proname IN
('hashtext','hashtextextended','hashbpchar','hashbpcharextended','hashname','hashnameextended',
 'hashchar','hashcharextended','hashbool','hashboolextended','hashint2','hashint2extended',
 'hashint4','hashint4extended','hashint8','hashint8extended','hashoid','hashoidextended',
 'hashfloat4','hashfloat4extended','hashfloat8','hashfloat8extended') ORDER BY oid;
