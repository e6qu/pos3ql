-- CLUSTER ON and REPLICA IDENTITY USING INDEX select primary key and unique
-- constraint indexes, which pg_index marks like any other index.
create table ck (a int primary key, b int not null unique, c int, d int not null, unique (c, d), e int, constraint ck_e unique (e) deferrable);
create index ck_c on ck (c);
alter table ck cluster on ck_pkey;
select c.relname, i.indisclustered, i.indisreplident from pg_index i join pg_class c on c.oid = i.indexrelid where i.indrelid = 'ck'::regclass order by 1;
alter table ck cluster on ck_c;
select c.relname, i.indisclustered from pg_index i join pg_class c on c.oid = i.indexrelid where i.indrelid = 'ck'::regclass order by 1;
cluster ck using ck_b_key;
select c.relname, i.indisclustered from pg_index i join pg_class c on c.oid = i.indexrelid where i.indrelid = 'ck'::regclass order by 1;
cluster ck;
cluster;
alter table ck set without cluster;
select c.relname, i.indisclustered from pg_index i join pg_class c on c.oid = i.indexrelid where i.indrelid = 'ck'::regclass order by 1;
cluster ck;
alter table ck replica identity using index ck_b_key;
select relreplident from pg_class where relname = 'ck';
select c.relname, i.indisreplident from pg_index i join pg_class c on c.oid = i.indexrelid where i.indrelid = 'ck'::regclass order by 1;
alter table ck replica identity using index ck_c_d_key;
alter table ck replica identity using index ck_e;
alter table ck replica identity using index ck_pkey;
select c.relname, i.indisreplident from pg_index i join pg_class c on c.oid = i.indexrelid where i.indrelid = 'ck'::regclass order by 1;
\d ck
alter table ck replica identity full;
select c.relname, i.indisreplident from pg_index i join pg_class c on c.oid = i.indexrelid where i.indrelid = 'ck'::regclass order by 1;
alter table ck cluster on ck_c_d_key;
alter table ck drop constraint ck_c_d_key;
select c.relname, i.indisclustered from pg_index i join pg_class c on c.oid = i.indexrelid where i.indrelid = 'ck'::regclass order by 1;
cluster ck;
drop table ck;
