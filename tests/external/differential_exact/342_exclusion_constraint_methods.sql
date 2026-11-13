-- Exclusion constraints over B-tree, hash, GiST and SP-GiST indexes, with the
-- operator checks and catalog rows PostgreSQL derives from the operator class.
create table eb (b box, EXCLUDE USING gist (b WITH &&));
insert into eb values ('((0,0),(1,1))');
insert into eb values ('((2,2),(3,3))');
insert into eb values ('((0.5,0.5),(2.5,2.5))');
select conname, contype, conexclop, pg_get_constraintdef(oid) from pg_constraint where conrelid = 'eb'::regclass;
select c.relname, a.amname, i.indclass::text from pg_index i join pg_class c on c.oid = i.indexrelid join pg_am a on a.oid = c.relam where i.indrelid = 'eb'::regclass;
create table et (a int, b text, EXCLUDE USING btree (a WITH =), EXCLUDE (b WITH =));
insert into et values (1, 'x');
insert into et values (1, 'y');
insert into et values (2, 'x');
select conname, contype, conexclop, pg_get_constraintdef(oid) from pg_constraint where conrelid = 'et'::regclass order by 1;
select c.relname, a.amname, i.indclass::text from pg_index i join pg_class c on c.oid = i.indexrelid join pg_am a on a.oid = c.relam where i.indrelid = 'et'::regclass order by 1;
create table er (a int4range, b int, EXCLUDE USING gist (a WITH &&));
insert into er values (int4range(1, 5), 1);
insert into er values (int4range(3, 8), 2);
insert into er values (int4range(5, 9), 3);
select conname, pg_get_constraintdef(oid) from pg_constraint where conrelid = 'er'::regclass;
create table ex (a int, EXCLUDE USING gist (a WITH =));
create table ex2 (a int, EXCLUDE USING btree (a WITH <>));
create table ex3 (a int4range, EXCLUDE USING btree (a WITH &&));
\d eb
\d et
drop table eb, et, er;
create table ex5 (a int, EXCLUDE USING btree (a WITH <));
create table ex6 (a int, EXCLUDE USING gin (a WITH =));
create table ex7 (a int, EXCLUDE USING hash (a WITH =));
insert into ex7 values (1), (2);
insert into ex7 values (1);
create table ex8 (p point, c circle, EXCLUDE USING gist (c WITH &&), EXCLUDE USING spgist (p WITH ~=));
insert into ex8 values ('(0,0)', '<(0,0),1>');
insert into ex8 values ('(5,5)', '<(1,0),1>');
insert into ex8 values ('(0,0)', '<(9,9),1>');
select conname, conexclop from pg_constraint where conrelid = 'ex8'::regclass order by 1;
alter table ex7 add constraint ex7_a_excl2 exclude using btree (a with =);
drop table ex7, ex8;
