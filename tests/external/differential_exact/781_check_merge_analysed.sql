-- A CHECK merges with a same-named one when their analysed predicates are
-- equal (`equal()` on the cooked trees): a cast to the type an operand
-- already has, or that analysis gives a literal, does not change the
-- predicate, while one that picks another operator does. A declared column
-- merged with an inherited one at another position is reported as moved.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/tablecmds.c
CREATE TABLE p_781 (a integer, b text, c numeric, d bigint, CONSTRAINT ck CHECK (a > 0), CONSTRAINT ck2 CHECK (b <> 'x'), CONSTRAINT ck3 CHECK (c > 1.5), CONSTRAINT ck4 CHECK (d > 0));
CREATE TABLE c1_781 (a integer, b text, c numeric, d bigint, CONSTRAINT ck CHECK (a > 0::integer), CONSTRAINT ck2 CHECK (b <> 'x'::text), CONSTRAINT ck3 CHECK (c > 1.5::numeric)) INHERITS (p_781);
CREATE TABLE c2_781 (a integer, CONSTRAINT ck CHECK (a::int4 > '0')) INHERITS (p_781);
CREATE TABLE c3_781 (d bigint, CONSTRAINT ck4 CHECK (d > 0::bigint)) INHERITS (p_781);
CREATE TABLE c4_781 (a integer, CONSTRAINT ck CHECK (CAST(a AS integer) > 0)) INHERITS (p_781);
CREATE TABLE c5_781 (b text, CONSTRAINT ck2 CHECK (b::text <> 'x')) INHERITS (p_781);
SELECT conrelid::regclass, conname, conislocal, coninhcount FROM pg_constraint WHERE contype = 'c' AND conrelid::regclass::text ~ '^(p|c[0-9])_781$' ORDER BY conrelid::regclass::text, 2;
CREATE TABLE q_781 (a integer, b text);
ALTER TABLE q_781 ADD CONSTRAINT ck CHECK (a > 0::int);
ALTER TABLE q_781 INHERIT p_781;
CREATE TABLE q2_781 (a integer, b text, c numeric, d bigint, CONSTRAINT ck CHECK (a > 0::smallint), CONSTRAINT ck2 CHECK (b <> 'x'), CONSTRAINT ck3 CHECK (c > 1.5), CONSTRAINT ck4 CHECK (d > 0));
ALTER TABLE q2_781 INHERIT p_781;
CREATE TABLE r_781 (a integer, b text);
CREATE TABLE r1_781 (a integer, b text, CONSTRAINT rk CHECK (a > 1::int)) INHERITS (r_781);
ALTER TABLE r_781 ADD CONSTRAINT rk CHECK (a > 1);
SELECT conrelid::regclass, conislocal, coninhcount FROM pg_constraint WHERE conname = 'rk' AND conrelid::regclass::text LIKE '%781' ORDER BY conrelid::regclass::text;
CREATE TABLE m1_781 (a integer, CONSTRAINT mk CHECK (a > 0));
CREATE TABLE m2_781 (a integer, CONSTRAINT mk CHECK (a > 0::integer));
CREATE TABLE m3_781 (a integer) INHERITS (m1_781, m2_781);
SELECT conname, conislocal, coninhcount FROM pg_constraint WHERE conrelid = 'm3_781'::regclass;
CREATE TABLE pp_781 (a integer, CONSTRAINT pk CHECK (a > 0)) PARTITION BY RANGE (a);
CREATE TABLE pp1_781 (a integer, CONSTRAINT pk CHECK (a > 0::integer));
ALTER TABLE pp_781 ATTACH PARTITION pp1_781 FOR VALUES FROM (1) TO (10);
SELECT conrelid::regclass, conislocal, coninhcount FROM pg_constraint WHERE conname = 'pk' AND conrelid::regclass::text LIKE '%781' ORDER BY conrelid::regclass::text;
CREATE TABLE pp2_781 PARTITION OF pp_781 (CONSTRAINT pk CHECK (a > 0::int)) FOR VALUES FROM (10) TO (20);
ALTER TABLE q_781 NO INHERIT p_781;
SELECT conislocal, coninhcount FROM pg_constraint WHERE conrelid = 'q_781'::regclass;
