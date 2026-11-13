-- UPDATE ONLY and DELETE FROM ONLY leave inheritance children out; a plain
-- target, `name *` and `ONLY (name)` follow PostgreSQL's relation syntax.
CREATE SCHEMA only_update_delete;
SET search_path = only_update_delete, public;
create table base(id int, label text);
create table child(extra int) inherits (base);
insert into base values (1, 'base');
insert into child values (2, 'child', 20);
update only base set label = label || '!';
select 'base' as relation, * from only base union all select 'child', id, label from child order by id;
update base * set label = upper(label);
select 'base' as relation, * from only base union all select 'child', id, label from child order by id;
update only (base) set id = id + 10 returning *;
delete from only base where id > 0;
select 'base' as relation, * from only base union all select 'child', id, label from child order by id;
insert into base values (3, 'again');
delete from base * where id = 3;
delete from only (base);
select count(*) from base;
delete from base;
select count(*) from base;
-- A view over ONLY its base writes the base table alone.
insert into base values (4, 'view');
insert into child values (5, 'view child', 50);
create view base_only as select id, label from only base;
update base_only set label = 'seen';
delete from base_only;
select 'base' as relation, * from only base union all select 'child', id, label from child order by id;
-- Partitioned targets: ONLY names the parent, which holds no rows.
create table parted(id int) partition by range (id);
create table parted_low partition of parted for values from (0) to (10);
insert into parted values (1);
update only parted set id = 2;
delete from only parted;
select * from parted;
-- Syntax PostgreSQL rejects.
update only base * set id = 0;
DROP SCHEMA only_update_delete CASCADE;
