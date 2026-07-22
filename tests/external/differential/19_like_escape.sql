-- The ESCAPE clause of LIKE and SIMILAR TO: the character that quotes a
-- literal % or _ in the pattern. Without it the escape character is a
-- backslash; an empty string disables escaping entirely.

SELECT 'abc' LIKE 'a$%c' ESCAPE '$';
SELECT 'a%c' LIKE 'a$%c' ESCAPE '$';
SELECT 'a_c' LIKE 'a$_c' ESCAPE '$';
SELECT 'abc' LIKE 'a%c' ESCAPE '$';
SELECT 'abc' NOT LIKE 'a$%c' ESCAPE '$';
SELECT 'ABC' ILIKE 'a$%c' ESCAPE '$';
SELECT 'ABC' ILIKE 'A$%C' ESCAPE '$';

-- an empty escape string means nothing quotes anything
SELECT 'abc' LIKE 'a%' ESCAPE '';
SELECT 'a%c' LIKE 'a%c' ESCAPE '';

-- NULL anywhere makes it unknown; a longer string is refused
SELECT 'abc' LIKE 'a%c' ESCAPE NULL;
SELECT 'abc' LIKE 'a%c' ESCAPE 'xy';
SELECT 'abc' SIMILAR TO 'a%c' ESCAPE 'xy';

-- SIMILAR TO takes the same clause
SELECT 'abc' SIMILAR TO 'a%c' ESCAPE '#';
SELECT 'a%c' SIMILAR TO 'a#%c' ESCAPE '#';
SELECT 'abc' SIMILAR TO 'a#%c' ESCAPE '#';
SELECT 'abc' NOT SIMILAR TO 'a#%c' ESCAPE '#';
SELECT 'a_c' SIMILAR TO 'a#_c' ESCAPE '#';

-- the default escape character is unchanged
SELECT 'abc' LIKE 'a%';
SELECT 'abc' LIKE '_bc';
SELECT 'abc' NOT LIKE 'x%';
SELECT NULL LIKE 'a';
SELECT 'a' LIKE NULL;
SELECT 'abc' SIMILAR TO '(a|b)bc';
SELECT 'abc' SIMILAR TO 'a_c';

CREATE TABLE likeesc (a text);
INSERT INTO likeesc VALUES ('a%c'), ('abc'), ('a_c');
SELECT a FROM likeesc WHERE a LIKE 'a$%c' ESCAPE '$' ORDER BY a;
SELECT a FROM likeesc WHERE a LIKE 'a%c' ORDER BY a;
SELECT a FROM likeesc WHERE a SIMILAR TO 'a#_c' ESCAPE '#' ORDER BY a;
DROP TABLE likeesc;
