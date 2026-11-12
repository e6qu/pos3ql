-- Foreign keys between columns of different types: PostgreSQL compares with
-- a cross-type operator of the referenced key's btree family, or casts the
-- referencing value to the key's type, and rejects other pairs (42804).
CREATE SCHEMA cross_type_foreign_keys;
SET search_path = cross_type_foreign_keys, public;
create table orders(id bigint primary key);
insert into orders select generate_series(1, 20);
create table lines(id int primary key, order_id integer references orders on delete cascade);
insert into lines values (1, 1), (2, 2), (3, 2), (4, null);
insert into lines values (5, 21);
update lines set order_id = 3 where id = 1;
update lines set order_id = 99 where id = 1;
delete from orders where id = 2;
select * from lines order by id;
-- A bigint key referencing an integer key.
create table small(id int primary key);
insert into small values (1), (2147483647);
create table big(k bigint references small on update cascade on delete set null);
insert into big values (1), (2147483647);
insert into big values (5000000000);
insert into big values (-5000000000);
update small set id = -1 where id = 1;
delete from small where id = 2147483647;
select * from big order by k;
-- A cascaded key that does not fit the referencing column.
create table wide(id bigint primary key);
insert into wide values (7);
create table narrow(k int references wide on update cascade);
insert into narrow values (7);
update wide set id = 5000000000;
update wide set id = 8;
select * from narrow;
-- Restrict and no action report the referenced key.
create table kinds(code smallint primary key);
insert into kinds values (1), (2);
create table things(kind bigint references kinds on delete restrict, other int8);
alter table things add foreign key (other) references kinds on update no action;
insert into things values (1, 2);
delete from kinds where code = 1;
update kinds set code = 3 where code = 2;
-- Casts to the referenced type: numeric and real keys, string keys with
-- trailing blanks, timestamps and object identifiers.
create table amounts(a numeric primary key);
insert into amounts values (1), (2.5);
create table uses_amounts(a int references amounts);
insert into uses_amounts values (1);
insert into uses_amounts values (2);
create table reals(r real primary key);
insert into reals values (16777216), (0.5);
create table uses_reals(r bigint references reals, f float8 references reals);
insert into uses_reals values (16777217, 0.5);
insert into uses_reals values (null, 0.1);
create table codes(c char(4) primary key);
insert into codes values ('ab');
create table uses_codes(t text references codes, v varchar(6) references codes);
insert into uses_codes values ('ab  ', 'ab');
insert into uses_codes values ('ab', 'ab ');
insert into uses_codes values (' ab', null);
create table words(w text primary key);
insert into words values ('ab'), ('ab ');
create table uses_words(c char(3) references words on update cascade, n name references words);
insert into uses_words values ('ab', 'ab');
insert into uses_words values ('ab ', 'ab ');
insert into uses_words values (null, 'zz');
update words set w = 'abcd' where w = 'ab';
select * from uses_words;
create table days(d date primary key);
insert into days values ('2024-01-01');
create table uses_days(t timestamp references days, z timestamptz references days);
insert into uses_days values ('2024-01-01 00:00', '2024-01-01 00:00+00');
insert into uses_days values ('2024-01-01 00:00:01', null);
insert into uses_days values (null, '2024-01-01 00:00+01');
create table oids(o oid primary key);
insert into oids values (4000000000);
create table uses_oids(o int8 references oids);
insert into uses_oids values (4000000000);
insert into uses_oids values (-1);
insert into uses_oids values (5000000000);
-- Domains compare through their base types.
create domain small_id as int;
create table uses_domain(k small_id references orders);
insert into uses_domain values (3);
insert into uses_domain values (2);
-- Validating existing rows and deferred checks use the same comparison.
create table later(k smallint);
insert into later values (4), (5);
alter table later add constraint later_k_fkey foreign key (k) references orders;
insert into later values (40);
alter table later drop constraint later_k_fkey;
alter table later add constraint later_k_fkey foreign key (k) references orders;
delete from later where k = 40;
alter table later add constraint later_k_fkey foreign key (k) references orders deferrable initially deferred;
begin;
insert into later values (30);
insert into orders values (30);
commit;
begin;
insert into later values (31);
commit;
-- Incompatible pairs.
create table bad1(k numeric references small);
create table bad2(k text references orders);
create table bad3(a int, b text, primary key (a, b));
create table bad4(x int8, y numeric, foreign key (x, y) references bad3);
create table bad5(x int8, y varchar);
alter table bad5 add constraint swapped foreign key (y, x) references bad3 (a, b);
create table bad6(k "char" references words);
-- Changing a key column's type keeps the key only while it stays
-- implementable.
create table ap(id bigint primary key);
create table ac(k int references ap);
insert into ap values (1);
insert into ac values (1);
alter table ac alter column k type numeric;
alter table ac alter column k type text;
alter table ac alter column k type smallint;
alter table ap alter column id type numeric;
alter table ap alter column id type int;
insert into ac values (2);
select conname, conpfeqop, conppeqop, conffeqop, confdelsetcols, conexclop
from pg_constraint
where contype = 'f' and connamespace = 'cross_type_foreign_keys'::regnamespace
order by conname;
DROP SCHEMA cross_type_foreign_keys CASCADE;
