-- A CHECK constraint merges with a same-named one a table inherits, gets
-- from a parent it joins, or receives from its partitioned parent when
-- their analysed expressions are equal (`equal()`), whatever the spacing,
-- parentheses or keyword case. A different expression, a NO INHERIT side or
-- a NOT VALID or NOT ENFORCED constraint conflicts with PostgreSQL's errors,
-- and each column merge is reported before such an error.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/heap.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/tablecmds.c
CREATE TABLE p_742 (a integer, b text, CONSTRAINT ck CHECK (a > 0));
CREATE TABLE c1_742 (a integer, b text, CONSTRAINT ck CHECK ((a>0))) INHERITS (p_742);
CREATE TABLE c2_742 (a integer, b text, CONSTRAINT ck CHECK (A  >  0)) INHERITS (p_742);
CREATE TABLE c3_742 (a integer, b text, CONSTRAINT ck CHECK (a >= 1)) INHERITS (p_742);
CREATE TABLE c4_742 (a integer, b text, CONSTRAINT ck CHECK (0 < a)) INHERITS (p_742);
CREATE TABLE c5_742 (a integer, b text, CONSTRAINT ck CHECK ("a" > 0)) INHERITS (p_742);
CREATE TABLE c6_742 (a integer, b text, CONSTRAINT ck CHECK (p_742.a > 0)) INHERITS (p_742);
SELECT conrelid::regclass, conname, conislocal, coninhcount, pg_get_constraintdef(oid) FROM pg_constraint WHERE contype = 'c' ORDER BY conrelid::regclass::text, conname;
CREATE TABLE q_742 (a integer, b text);
ALTER TABLE q_742 ADD CONSTRAINT ck CHECK ( ( a > 0 ) );
ALTER TABLE q_742 INHERIT p_742;
SELECT conislocal, coninhcount FROM pg_constraint WHERE conrelid = 'q_742'::regclass;
CREATE TABLE r_742 (a integer, b text, CONSTRAINT ck CHECK (a > 1));
ALTER TABLE r_742 INHERIT p_742;
CREATE TABLE r2_742 (a integer, b text, CONSTRAINT ck CHECK (a > 0) NO INHERIT);
ALTER TABLE r2_742 INHERIT p_742;
CREATE TABLE p2_742 (a integer, b text);
CREATE TABLE k_742 (a integer, b text, CONSTRAINT ck2 CHECK (length(b)>2)) INHERITS (p2_742);
ALTER TABLE p2_742 ADD CONSTRAINT ck2 CHECK (length( b ) > 2);
SELECT conrelid::regclass, conislocal, coninhcount FROM pg_constraint WHERE conname = 'ck2' ORDER BY 1::text;
ALTER TABLE p2_742 ADD CONSTRAINT ck3 CHECK (b <> 'x');
ALTER TABLE ONLY k_742 ADD CONSTRAINT ck4 CHECK (b<>'y');
ALTER TABLE p2_742 ADD CONSTRAINT ck4 CHECK (b <> 'y');
SELECT conrelid::regclass, conname, conislocal, coninhcount FROM pg_constraint WHERE conname IN ('ck3', 'ck4') ORDER BY 1::text, 2;
CREATE TABLE pp_742 (a integer, CONSTRAINT cc CHECK (a > 0)) PARTITION BY RANGE (a);
CREATE TABLE pp1_742 (a integer, CONSTRAINT cc CHECK ((a > 0)));
ALTER TABLE pp_742 ATTACH PARTITION pp1_742 FOR VALUES FROM (1) TO (10);
CREATE TABLE pp2_742 (a integer, CONSTRAINT cc CHECK (a > 5));
ALTER TABLE pp_742 ATTACH PARTITION pp2_742 FOR VALUES FROM (10) TO (20);
SELECT conrelid::regclass, conislocal, coninhcount FROM pg_constraint WHERE conname = 'cc' ORDER BY 1::text;
CREATE TABLE p1_742b (a integer, CONSTRAINT ck CHECK (a > 0));
CREATE TABLE p2_742b (a integer, CONSTRAINT ck CHECK (a > 1));
CREATE TABLE p3_742b (a integer, CONSTRAINT ck CHECK ((a)>0));
CREATE TABLE c_742b () INHERITS (p1_742b, p2_742b);
CREATE TABLE c_742b () INHERITS (p1_742b, p3_742b);
SELECT conname, conislocal, coninhcount FROM pg_constraint WHERE conrelid = 'c_742b'::regclass;
CREATE TABLE d_742b (a integer, CONSTRAINT ck CHECK (a > 0) NO INHERIT) INHERITS (p1_742b);
CREATE TABLE e_742b (a integer, CONSTRAINT ck CHECK (a > 0));
CREATE TABLE f_742b (a integer) INHERITS (e_742b);
ALTER TABLE f_742b ADD CONSTRAINT ck CHECK (a > 0) NO INHERIT;
ALTER TABLE ONLY f_742b ADD CONSTRAINT ck2 CHECK (a > 0) NO INHERIT;
ALTER TABLE e_742b ADD CONSTRAINT ck2 CHECK (a > 0);
ALTER TABLE e_742b ADD CONSTRAINT ck3 CHECK (a > 0);
ALTER TABLE e_742b ADD CONSTRAINT ck3 CHECK (a > 0);
CREATE TABLE g_742b (a integer, CONSTRAINT ck CHECK (a > 0)) INHERITS (p2_742b);
CREATE TABLE nv_742 (a integer);
CREATE TABLE nvc_742 (a integer, CONSTRAINT v CHECK (a > 0) NOT VALID);
ALTER TABLE nvc_742 INHERIT nv_742;
ALTER TABLE nv_742 ADD CONSTRAINT v CHECK (a > 0);
ALTER TABLE nvc_742 INHERIT nv_742;
CREATE TABLE ne_742 (a integer, CONSTRAINT w CHECK (a > 0) NOT ENFORCED);
CREATE TABLE nec_742 (a integer, CONSTRAINT w CHECK (a > 0)) INHERITS (ne_742);
CREATE TABLE nec2_742 (a integer, CONSTRAINT w CHECK (a > 0) NOT ENFORCED) INHERITS (p_742);
CREATE TABLE nec3_742 (a integer, CONSTRAINT ck CHECK (a > 0) NOT ENFORCED) INHERITS (p_742);
SELECT conrelid::regclass, conname, conenforced, convalidated, conislocal, coninhcount FROM pg_constraint WHERE conrelid::regclass::text LIKE 'n%742' ORDER BY 1::text, 2;
ALTER TABLE p_742 INHERIT c1_742;
ALTER TABLE c1_742 INHERIT c1_742;
CREATE TABLE dup_742 (a integer) INHERITS (p_742, q_742, p_742);
