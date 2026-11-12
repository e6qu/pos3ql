-- pg_get_viewdef prints enum literals as constants of their type and follows
-- renamed tables, functions, types and enum values.
create schema vr;
create function vr.f(int) returns int language sql immutable as 'select $1 + 1';
create type vr_e as enum ('a', 'b');
create table vr_t (x vr_e, k int);
create view vr_filter as select x from vr_t where x = 'a';
create view vr_cast as select 'b'::vr_e as c, k from vr_t;
create view vr_list as select k from vr_t where x in ('a', 'b');
create view vr_call as select vr.f(k) as a, k::text::vr_e::text as b from vr_t;
select pg_get_viewdef('vr_filter'::regclass), pg_get_viewdef('vr_cast'::regclass);
select pg_get_viewdef('vr_list'::regclass), pg_get_viewdef('vr_call'::regclass);
alter type vr_e rename value 'a' to 'z';
alter type vr_e rename to vr_e2;
alter function vr.f(int) rename to f2;
alter table vr_t rename to vr_t2;
select pg_get_viewdef('vr_filter'::regclass), pg_get_viewdef('vr_cast'::regclass);
select pg_get_viewdef('vr_call'::regclass);
drop view vr_filter, vr_cast, vr_list, vr_call;
drop table vr_t2;
drop type vr_e2;
drop schema vr cascade;
