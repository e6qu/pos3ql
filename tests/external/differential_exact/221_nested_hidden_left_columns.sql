-- A subquery nested inside a derived table, or inside that table's own FROM
-- item, cannot see the items to the derived table's left either: PostgreSQL
-- explains the hidden column and suggests LATERAL.
CREATE SCHEMA nested_hidden_left_columns;
SET search_path = nested_hidden_left_columns, public;
create table lt(v int);
select * from lt, (select (select v)) s;
select * from lt, (select 1 from (select (select v)) x) s;
select * from lt, (select 1 where exists (select v)) s;
select * from lt, (select (select v) from lt l2) s;
