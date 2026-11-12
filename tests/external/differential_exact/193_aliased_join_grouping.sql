-- An ungrouped column of an aliased parenthesized join is named by its member table.
create table ga193(x int, z int); create table gb193(y int);
select x from (ga193 a join gb193 b on true) j group by j.y;
select j.x from (ga193 a join gb193 b on true) j group by j.y;
select x from (ga193 join gb193 on true) j group by y;
select z from (ga193 a join gb193 b on true) j group by x;
select x from (ga193 a join gb193 b on true) group by y;
select j.x from (select * from ga193) j group by j.z;
select x from ((ga193 a join gb193 b on true) k join gb193 c on true) j group by j.y;
drop table ga193, gb193;
