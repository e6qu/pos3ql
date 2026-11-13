-- A data change reads the catalog as it began, even after a trigger it fires
-- creates tables.
create function mk350(i int) returns int language plpgsql as $$
begin execute format('create table made350_%s (a int)', i); return i; end $$;
create function trg350() returns trigger language plpgsql as $$
begin perform mk350(new.g); return new; end $$;
create table src350 (g int);
insert into src350 values (1), (2);
create table dst350 (g int, n bigint);
create trigger t350 before insert on dst350 for each row execute function trg350();
insert into dst350
  select g, (select count(*) from pg_class where relname like 'made350%') from src350;
select * from dst350 order by 1;
create table upd350 (g int, n bigint);
insert into upd350 values (3, 0), (4, 0);
create trigger u350 before update on upd350 for each row execute function trg350();
update upd350 set n = (select count(*) from pg_class where relname like 'made350%');
select * from upd350 order by 1;
select relname from pg_class where relname like 'made350%' order by 1;
drop table dst350, upd350, src350, made350_1, made350_2, made350_3, made350_4;
drop function trg350(), mk350(int);
