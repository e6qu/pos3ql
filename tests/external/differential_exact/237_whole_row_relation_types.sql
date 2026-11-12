-- Whole-row references in DML conditions and RETURNING have the target
-- relation's row type, in both the t.* and bare-name spellings.
create table wt (a int, b text);
insert into wt values (1, 'x'), (2, 'y'), (3, 'z');
create function fw(wt) returns int language sql immutable as 'select $1.a * 10';
update wt set b = 'q' where fw(wt.*) = 20 returning fw(wt.*), wt.*;
update wt set b = 'r' where fw(wt.*) = 30 returning fw(wt);
insert into wt values (4, 'w') returning fw(wt.*);
select fw(wt.*), fw(wt) from wt order by 1;
delete from wt where fw(wt.*) = 10 returning fw(wt);
delete from wt where fw(wt) = 20 returning a;
update wt set b = 'q' where fw(wt) = 30 returning fw(wt), wt;
select fw(wt.*), fw(wt) from wt order by 1;
