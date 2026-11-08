CREATE TYPE layout_pair AS(id integer,label text);
CREATE TABLE layout_source(id integer,label text,grp integer);
INSERT INTO layout_source VALUES(2,'two',1),(1,'one',1),(3,'three',2);
CREATE SEQUENCE layout_counter;
SELECT
    ARRAY(SELECT tail FROM (
        SELECT (ROW(id,label)::layout_pair).*,id+10 AS tail
        FROM layout_source ORDER BY 3 DESC
    ) q)=ARRAY[13,12,11],
    ARRAY(SELECT tail FROM (
        SELECT (ROW(id,label)::layout_pair).*,id+10 AS tail
        FROM layout_source ORDER BY tail DESC LIMIT 2 OFFSET 1
    ) q)=ARRAY[12,11],
    ARRAY(SELECT n FROM (
        SELECT (ROW(id,label)::layout_pair).*,row_number() OVER(ORDER BY id) AS n
        FROM layout_source ORDER BY 3 DESC
    ) q)=ARRAY[3,2,1]::bigint[],
    ARRAY(SELECT n FROM (
        SELECT (ROW(grp,'group')::layout_pair).*,count(*) AS n
        FROM layout_source GROUP BY 1,2 ORDER BY 3 DESC,1
    ) q)=ARRAY[2,1]::bigint[],
    ARRAY(SELECT n FROM (
        SELECT (ROW(7,'seven')::layout_pair).*,count(*) AS n GROUP BY 1,2
    ) q)=ARRAY[1]::bigint[],
    ARRAY(SELECT (q.extra).f1 FROM (
        SELECT (ROW(id,label)::layout_pair).*,ROW(id+10,label) AS extra
        FROM layout_source
    ) q ORDER BY q.id)=ARRAY[11,12,13],
    ARRAY(SELECT (u.tail).f1 FROM (
        SELECT (ROW(1,'one')::layout_pair).*,ROW(9,'nine'::text) AS tail
        UNION ALL
        SELECT (ROW(2,'two')::layout_pair).*,ROW(8,'eight'::text) AS tail
    ) u ORDER BY u.id)=ARRAY[9,8],
    ARRAY(SELECT tail FROM (
        SELECT DISTINCT (ROW(id,label)::layout_pair).*,label COLLATE "C" AS tail
        FROM layout_source ORDER BY 3
    ) q)=ARRAY['one','three','two']::text[],
    ARRAY(SELECT stamp FROM (
        SELECT (ROW(id,label)::layout_pair).*,nextval('layout_counter') AS stamp
        FROM layout_source ORDER BY id DESC OFFSET 1 LIMIT 1
    ) q)=ARRAY[2]::bigint[];
SELECT currval('layout_counter');
