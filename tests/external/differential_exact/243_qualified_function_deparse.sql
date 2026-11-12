-- A user function the bare name would not select is printed schema-qualified
-- in index keys, predicates and CHECK constraints, also after its schema is
-- renamed.
create schema sx;
create function sx.f(int) returns int language sql immutable as 'select $1 + 1';
create table ti(a int);
create index ti_f on ti ((sx.f(a)));
create index ti_g on ti (a) where sx.f(a) > 0;
alter table ti add constraint ti_c check (sx.f(a) > 0);
select pg_get_indexdef('ti_f'::regclass), pg_get_indexdef('ti_g'::regclass);
select pg_get_constraintdef(oid) from pg_constraint where conname = 'ti_c';
alter schema sx rename to sy;
select pg_get_indexdef('ti_f'::regclass), pg_get_indexdef('ti_g'::regclass);
select pg_get_constraintdef(oid) from pg_constraint where conname = 'ti_c';
drop table ti;
drop schema sy cascade;
