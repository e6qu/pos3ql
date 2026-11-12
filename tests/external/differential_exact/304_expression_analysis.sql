-- Window definitions and DISTINCT ON keys are analysed with the query, so an
-- unknown column fails even over no rows, and an unknown-type literal that
-- an operator cannot convert is positioned. Generation expressions and
-- defaults are analysed when the table is created (positioned only by
-- CREATE TABLE).
create table t304 (abc int, xyz text, abe int);
select sum(abc) over (order by abd) from t304;
select sum(abc) over w from t304 window w as (partition by abd);
select distinct on (abd) * from t304;
select distinct on (abc) * from t304 order by abd;
select distinct on (x) abc as x from t304;
select abc from t304 group by abd;
select row_number() over (partition by 1 + 'x') from t304;
select distinct on (1 + 'x') * from t304;
insert into t304 values (1, 'a', 2);
select sum(abc) over (partition by abd) from t304;
select distinct on (abd) * from t304;
select * from t304 where 1 + 'x' = 2;
select * from t304 order by 1 + 'x';
select * from t304 group by 1 + 'x';
-- Generation expressions.
create table g1_304 (abc int, d int generated always as (abd + 1) stored);
create table g4_304 (abc int, d int generated always as ('x'::text) stored);
create table g5_304 (abc int, d int generated always as (abc || 'x') stored);
create table g6_304 (abc int, d int generated always as (abc + 'x') stored);
create table g7_304 (abc int, d int generated always as (g7_304.abc + 1) stored);
create table g8_304 (abc int, d int generated always as (zz.abc + 1) stored);
create table g9_304 (abc int, d int generated always as (abc + 1.5) stored);
insert into g9_304 values (1);
select * from g9_304;
create table g10_304 (abc int, d text generated always as (abc) stored);
create table g11_304 (abc int);
alter table g11_304 add column d int generated always as (abd * 2) stored;
alter table g11_304 add column d int generated always as (1 + 'x') stored;
alter table g11_304 add column d int generated always as ('x'::text) stored;
alter table g11_304 add column e int generated always as (abc * 2) stored;
alter table g11_304 alter column e set expression as (abd + 1);
create table g12_304 (like g11_304, d int generated always as (abd + 1) stored);
-- Defaults.
create table d1_304 (abc int default 'x'::text);
create table d2_304 (abc int default 'x');
create table d3_304 (abc int default (1 + 'x'));
create table d4_304 (abc date default now());
create table d5_304 (abc int default 1.5);
insert into d5_304 default values;
select * from d5_304;
create table d6_304 (abc int, b int default abc);
create table d7_304 (abc int, b int default (t.abc));
create table d8_304 (abc int);
alter table d8_304 alter column abc set default 'x'::text;
alter table d8_304 alter column abc set default 'x';
alter table d8_304 alter column abc set default abc;
alter table d8_304 add column d int default 'x';
alter table d8_304 add column d int default abc;
alter table d8_304 add column d int check (abd > 0);
create view v304 as select 1 as a;
alter view v304 alter column a set default 'x'::text;
create table d9_304 (abc text default 1);
create table d10_304 (abc int default 1.5::numeric);
create table d11_304 (abc varchar(2) default 'xyz');
insert into d11_304 default values;
create table d12_304 (abc numeric(3,1) default 12345.6);
insert into d12_304 default values;
create table d13_304 (like d8_304, d int default 'x');
-- CHECK constraints.
create table c1_304 (abc int check (abd > 0));
create table c2_304 (abc int, check (abd > 0));
alter table c2_304 add constraint c check (abd > 0);
