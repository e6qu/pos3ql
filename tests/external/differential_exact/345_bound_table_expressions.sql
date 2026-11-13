-- CHECK constraints, defaults, generated columns and index keys and
-- predicates stay bound to the routines and types they named when written:
-- a later search_path, rename or schema move changes nothing, and dropping
-- what they name reports and cascades to them as PostgreSQL does.
create schema s20;
create function s20.f(x int) returns int language sql immutable return x * 2;
create type s20.e as enum ('a', 'b');
set search_path = s20, public;
create table public.t20 (a int check (f(a) > 0), b int default f(5), c int generated always as (f(a) + 1) stored, d text default 'a'::e::text);
create table public.t20b (a int, constraint t20b_chk check (f(a) < 100));
reset search_path;
insert into t20 (a) values (1);
select * from t20;
insert into t20 (a) values (-1);
select pg_get_constraintdef(oid) from pg_constraint where conrelid = 't20'::regclass and contype = 'c';
select pg_get_expr(adbin, adrelid) from pg_attrdef where adrelid = 't20'::regclass order by adnum;
alter function s20.f(int) rename to g;
insert into t20 (a) values (2);
select * from t20 order by a;
select pg_get_constraintdef(oid) from pg_constraint where conrelid = 't20'::regclass and contype = 'c';
select pg_get_expr(adbin, adrelid) from pg_attrdef where adrelid = 't20'::regclass order by adnum;
alter schema s20 rename to s21;
insert into t20 (a) values (3);
select pg_get_constraintdef(oid) from pg_constraint where conrelid in ('t20'::regclass, 't20b'::regclass) and contype = 'c' order by 1;
alter table t20b rename constraint t20b_chk to t20b_chk2;
insert into t20b values (200);
alter type s21.e rename to e2;
insert into t20 (a) values (4);
select a, b, c, d from t20 order by a;
drop function s21.g(int);
drop table t20, t20b;
drop schema s21 cascade;
create function fz(int) returns int language sql immutable return 1;
create table tz(a int check (fz(a) > 0), b int default fz(2));
select classid::regclass, refclassid::regclass, deptype from pg_depend where refobjid = 'fz(int)'::regprocedure order by 1;
drop function fz(int);
drop function fz(int) cascade;
\d tz
drop table tz;
create function fg(int) returns int language sql immutable return $1;
create table tg(a int, g int generated always as (fg(a)) stored, c int check (fg(c) > 0), d int default fg(1));
drop function fg(int);
drop function fg(int) cascade;
\d tg
drop table tg;
create type en as enum ('x','y');
create domain dm as int check (value > 0);
create table te(a en check (a <> 'y'::en), b int default 1::dm, c int check (c::dm > 0));
drop type en;
drop domain dm;
drop type en cascade;
drop domain dm cascade;
\d te
drop table te;
create schema sx;
create function sx.h(int) returns int language sql immutable return $1;
create table th(a int check (sx.h(a) > 0), b int);
drop schema sx;
drop schema sx cascade;
\d th
drop table th;
create schema q;
create sequence q.sq;
set search_path = q, public;
create table ts(a int default nextval('sq'), b int check (b > currval('sq')), c text default 'x'::text);
reset search_path;
alter sequence q.sq rename to sq2;
alter schema q rename to q2;
insert into ts(b) values (-5);
select * from ts;
\d ts
create table ts2(a serial, b int);
alter sequence ts2_a_seq rename to zz;
insert into ts2(b) values (1);
\d ts2
drop table ts, ts2; drop schema q2 cascade;
create schema s25;
create function s25.k(int) returns int language sql immutable return $1 % 10;
set search_path = s25, public;
create table public.ti(a int, b int);
create unique index ti_k on ti (k(a)) where k(b) > 0;
create index ti_k2 on ti (k(b));
reset search_path;
insert into ti values (1, 1);
insert into ti values (11, 1);
insert into ti values (11, 0);
select pg_get_indexdef('ti_k'::regclass), pg_get_indexdef('ti_k2'::regclass);
alter function s25.k(int) rename to k2;
alter schema s25 rename to s26;
insert into ti values (21, 3);
select pg_get_indexdef('ti_k'::regclass);
\d ti
select classid::regclass, refclassid::regclass, deptype from pg_depend where objid = 'ti_k'::regclass order by 1,2,3;
drop function s26.k2(int);
drop function s26.k2(int) cascade;
\d ti
drop table ti; drop schema s26 cascade;
create table tu(a int, b text);
create unique index on tu ((a + 1));
create unique index on tu (lower(b));
create unique index on tu ((b || 'x'), a);
insert into tu values (1, 'A');
insert into tu values (1, 'b');
insert into tu values (2, 'a');
insert into tu values (3, 'A');
drop table tu;
