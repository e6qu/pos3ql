-- PostgreSQL runs a type's input routine on an untyped literal while it
-- analyses the statement, so the literal fails even when no row reads it.
create table li_t (a int);
create table li_reg (r regclass);
create type li_mood as enum ('ok');
create table li_m (m li_mood);
create sequence li_seq;
-- Object-identifier parameters and casts.
select nextval('li_missing') from li_t;
select 1 from li_t where nextval('li_missing') > 0;
select currval('li_missing') from li_t;
select setval('li_missing', 1) from li_t;
select pg_relation_size('li_missing') from li_t;
select case when false then nextval('li_missing') end;
select (select nextval('li_missing') from li_t);
select 1 from li_t where exists (select nextval('li_missing'));
select 'li_missing'::regclass from li_t;
select case when false then 'li_missing'::regclass end;
select '{li_missing}'::regclass[] from li_t;
select array['li_missing']::regclass[] from li_t;
select 'li_nosuchtype'::regtype from li_t;
select 'li_nosuchfn'::regproc from li_t;
select 'li_nosuchfn(int)'::regprocedure from li_t;
select 'li_nosuchns'::regnamespace from li_t;
select 'li_nosuchrole'::regrole from li_t;
select 'li_nosuchcfg'::regconfig from li_t;
select 'li_nosuchdict'::regdictionary from li_t;
select 'li_nosuchcoll'::regcollation from li_t;
select nextval('li_seq'::text) from li_t;
select has_table_privilege('li_missing', 'select') from li_t;
-- regoper and regoperator input.
select 'li_nosuchop'::regoper from li_t;
select '+'::regoper;
select '@?'::regoper;
select '+'::regoperator;
select '+(int4)'::regoperator;
select '+(int4,int4,int4)'::regoperator;
select '+()'::regoperator;
select '+(int4,li_nosuch)'::regoperator;
select 'li_nosuchop(int4,int4)'::regoperator;
select '+(int4,int4'::regoperator;
select '+(int4, int4)'::regoperator, '-(NONE,int4)'::regoperator, 'pg_catalog.+(int4,int4)'::regoperator;
select 'li_nosuch.+(int4,int4)'::regoperator;
select 'li_nosuch.+'::regoper;
select 'li_f(li_nosuch)'::regprocedure;
select 'li_f(int'::regprocedure;
select 'li_f'::regprocedure;
-- Object identifiers compare as oid.
select 1 from li_reg where r = 'li_missing';
select 'pg_class'::regclass = 'pg_class';
select 'pg_class'::regclass = '1259';
-- Other types, in live and dead code.
select case when false then 'abc'::int end;
select 1 from li_t where false and 'abc'::int = 1;
select 1 from li_t where sqrt('abc') > 0;
-- Assignments, COPY WHERE and defaults.
insert into li_reg select 'li_missing' from li_t;
insert into li_reg values ('li_missing');
update li_reg set r = 'li_missing' where false;
insert into li_t select 'abc' from li_t;
update li_t set a = 'abc' where false;
insert into li_m select 'bad' from li_t;
update li_m set m = 'bad' where false;
insert into li_t select nextval('li_missing') from li_t;
update li_t set a = nextval('li_missing') where false;
delete from li_t where a = nextval('li_missing');
copy li_t from stdin where nextval('li_missing') > 0;
\.
copy li_t from stdin where 'li_missing'::regclass is null;
\.
copy li_t from stdin where a = 'abc';
\.
create table li_e (a int default 'abc');
create table li_e (a int default nextval('li_missing'));
create table li_e (a int default nextval('li_seq'));
drop table li_e;
prepare li_p as select nextval('li_missing') from li_t;
drop table li_t, li_reg, li_m;
drop type li_mood;
drop sequence li_seq;
