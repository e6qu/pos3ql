-- A table hidden by its alias is an invalid reference, with PostgreSQL's hint
-- naming the alias; jsonb_each names itself for a non-object.
CREATE SCHEMA aliased_relation_references;
SET search_path = aliased_relation_references, public;
create table ts_a(id int, x text);
select ts_a.* from ts_a t;
select ts_a.id from ts_a t;
select t.id from ts_a t where ts_a.id = 1;
select nosuch.* from ts_a t;
select jsonb_each('5'::jsonb);
select * from jsonb_each_text('[1]');
select json_each('[1]');
select json_each('5');
