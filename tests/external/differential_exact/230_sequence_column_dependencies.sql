-- Column-level dependencies on sequences, types named in defaults, and
-- sequence column identities.
create sequence sq;
create view vs as select last_value, is_called from sq;
create view vs2 as select * from sq;
create view vs3 as select nextval('sq');
create domain dd as int;
create table td (a int default 1::dd);
create function fd() returns int language sql as 'select 1::dd';
create function fd2() returns int language sql begin atomic select 1::dd; end;
create view vd as select 1::dd as x;
select classid::regclass, objsubid, refclassid::regclass, refobjsubid, deptype, pg_describe_object(classid,objid,objsubid) obj, pg_describe_object(refclassid,refobjid,refobjsubid) ref from pg_depend where objid >= 16384 and refobjid >= 16384 order by obj, ref;
DROP DOMAIN dd;
create view vs4 as select s.last_value from sq s;
create view vs5 as select sq.is_called, nextval('sq') from sq;
select pg_describe_object(classid,objid,objsubid) obj, pg_describe_object(refclassid,refobjid,refobjsubid) ref from pg_depend where refobjid = 'sq'::regclass and classid = 'pg_rewrite'::regclass order by obj, ref;
select * from pg_identify_object('pg_class'::regclass, 'sq'::regclass, 1);
select * from pg_identify_object_as_address('pg_class'::regclass, 'sq'::regclass, 2);
select pg_get_object_address('sequence column', '{public,sq,is_called}', '{}') = ('pg_class'::regclass, 'sq'::regclass, 3);
select * from pg_identify_object('pg_class'::regclass, 'sq'::regclass, 4);
select pg_get_object_address('view column', '{public,vs,last_value}', '{}');
select pg_get_object_address('toast table', '{a}', '{}');
select pg_get_object_address('bogus', '{a}', '{}');
DROP VIEW vs, vs2, vs3, vs4, vs5, vd;
DROP TABLE td;
DROP FUNCTION fd(), fd2();
DROP DOMAIN dd;
DROP SEQUENCE sq;
