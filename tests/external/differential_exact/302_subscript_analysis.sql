-- Subscripting a type without subscripts fails while the statement is
-- analysed, positioned at the subscripted value; an UPDATE target is
-- positioned too. Only variable-length arrays have slices.
create table t302 (a int, b int[], j json, jb jsonb, p point, n name, tx text);
insert into t302 values (1, '{1,2}', '[1]', '[1]', '(1,2)', 'ab', 'x');
create domain d302 as int;
create domain da302 as int[];
select '{1,2}'[1];
select j[0] from t302;
select jb[0], p[0], n[0], b[1], b[1:1] from t302;
select tx[1] from t302;
select (1::d302)[1];
select ('{1}'::da302)[1];
select (row(1))[1];
select * from t302 where a[1] = 1;
select array_agg(a)[1] from t302;
select (array_agg(a))[1] from t302;
select jb[1:2] from t302;
select p[0:1] from t302;
update t302 set a[1] = 2;
select (select b from t302)[1], (select b from t302)[1:1];
select coalesce(a, 1)[1] from t302;
select (a + 1)[1] from t302;
select (tx || 'x')[1] from t302;
insert into t302 (a[1]) values (1);
update t302 set tx[1] = 'x';
update t302 set p[0] = 5;
select p from t302;
update t302 set j[0] = '1';
create type comp302 as (x int);
create table tc302 (c comp302);
update tc302 set c[1] = 1;
