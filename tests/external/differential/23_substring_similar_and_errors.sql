-- `substring(x SIMILAR p ESCAPE e)` is SQL:2003's spelling of the SQL-regular-
-- expression form that `FROM p FOR e` already spells, so both reach the same
-- extraction; and an undefined function is reported with the argument types it
-- was called with, as PostgreSQL does.

SELECT substring('abcd' similar 'a#"bc#"d' escape '#');
SELECT substring('abcd' from 'a#"bc#"d' for '#');
SELECT substring('abc' similar 'abc' escape '#');
SELECT substring('abc' similar 'x#"y#"z' escape '#');
SELECT substring('foobar' similar '%#"o+#"_' escape '#');
SELECT substring('abcd' similar 'abcd' escape '#');

-- exactly zero or two markers; a third is refused by name rather than reaching
-- the regex engine as an unbalanced parenthesis
SELECT substring('abcd' similar 'a#"b#"%#"' escape '#');
SELECT substring('abcd' from 'a#"b#"%#"' for '#');

-- the other substring spellings are untouched
SELECT substring('abcd' from 2);
SELECT substring('abcd' from 2 for 2);
SELECT substring('abcd', 2, 2);
SELECT substring('abcd' for 2);
SELECT substring('abcd' from 'b.');
SELECT substring('ab'||'cd' from 2);
SELECT 'abcd' SIMILAR TO 'a%';
SELECT 'abcd' SIMILAR TO 'a%' ESCAPE '#';

-- an undefined function names the types it was called with; an untyped literal
-- is `unknown` however it would later coerce
SELECT nosuchfunc(1);
SELECT nosuchfunc('a');
SELECT nosuchfunc();
SELECT nosuchfunc(1,'a');
SELECT nosuchfunc(1::int8, true);
SELECT nosuchfunc(1.5);
SELECT nosuchfunc(ARRAY[1]);
SELECT nosuchfunc(ARRAY['a']);
SELECT similar_to('a','a');
SELECT overlaps(1,2,3,4);
