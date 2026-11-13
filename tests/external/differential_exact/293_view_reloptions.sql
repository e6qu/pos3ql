-- View reloptions keep their written order and spelling, list an explicit
-- security_invoker=false, and move options set by ALTER VIEW to the end.
create table vt(a int);
create view v1 with (check_option=local, security_barrier=true, security_invoker=false) as select * from vt;
select reloptions from pg_class where relname = 'v1';
alter view v1 set (security_invoker=true);
select reloptions from pg_class where relname = 'v1';
alter view v1 set (check_option=cascaded);
select reloptions from pg_class where relname = 'v1';
alter view v1 reset (security_barrier);
select reloptions from pg_class where relname = 'v1';
alter view v1 reset (unknown_option, security_invoker, security_invoker);
select reloptions from pg_class where relname = 'v1';
create or replace view v1 with (security_barrier=false) as select * from vt;
select reloptions from pg_class where relname = 'v1';
create or replace view v1 as select * from vt with local check option;
select reloptions from pg_class where relname = 'v1';
create view v2 with (security_barrier) as select * from vt with check option;
select reloptions from pg_class where relname = 'v2';
create view v3 with (security_invoker=false) as select * from vt;
select reloptions, relname from pg_class where relname = 'v3';
select viewname, definition from pg_views where viewname = 'v3';
create view v4 with (check_option=local) as select * from vt with cascaded check option;
create view v4 with (security_barrier=true, security_barrier=false) as select * from vt;
create view v4 with (no_such_option=1) as select * from vt;
create view v4 with (check_option=sideways) as select * from vt;
alter view v1 set (security_barrier=true, security_barrier=false);
select relname, reloptions from pg_class where relname in ('v1', 'v2', 'v3') order by 1;
-- Values keep their written spelling.
create view v5 with (check_option='LOCAL', security_barrier=TRUE, security_invoker='tRu') as select * from vt;
select reloptions from pg_class where relname = 'v5';
alter view v5 set (security_barrier='ye', security_invoker=0, check_option="cascaded");
select reloptions from pg_class where relname = 'v5';
alter view v5 set (security_barrier=of);
select reloptions from pg_class where relname = 'v5';
create view v6 with (security_barrier=' on') as select * from vt;
drop table vt cascade;
