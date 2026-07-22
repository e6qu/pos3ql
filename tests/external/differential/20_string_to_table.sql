-- `string_to_table(string, delimiter [, null_string])` yields one row per
-- piece, in the select list and in FROM. The split rule is the one
-- `string_to_array` uses, so the pair are checked together here: a NULL
-- delimiter splits into characters, an empty one does not split at all, an
-- empty input yields nothing, and a piece equal to null_string is NULL.

SELECT string_to_table('a,b,c', ',');
SELECT string_to_table('abc', NULL);
SELECT string_to_table('abc', '');
SELECT string_to_table(NULL, ',');
SELECT string_to_table('', ',');
SELECT string_to_table('a,,b', ',');
SELECT string_to_table('a,b,c', ',', 'b');
SELECT 1, string_to_table('a,b', ',');
SELECT upper(string_to_table('a,b', ','));
SELECT string_to_table('a,b', ',') || 'x';

SELECT * FROM string_to_table('a,b,c', ',');
SELECT * FROM string_to_table('abc', NULL);
SELECT * FROM string_to_table('abc', '');
SELECT * FROM string_to_table(NULL, ',');
SELECT * FROM string_to_table('', ',');
SELECT * FROM string_to_table('a,,b', ',');
SELECT * FROM string_to_table('a,b,c', ',', 'b');
SELECT * FROM string_to_table('a,b', ',') WITH ORDINALITY;
SELECT * FROM string_to_table('a,b', ',') AS t(v);
SELECT v FROM string_to_table('a,b', ',') AS t(v) ORDER BY v DESC;
SELECT count(*) FROM string_to_table('a,b,c', ',');

-- the array form must agree piece for piece
SELECT string_to_array('a,b,c', ',');
SELECT string_to_array('abc', NULL);
SELECT string_to_array('abc', '');
SELECT string_to_array('', ',');
SELECT string_to_array('a,,b', ',');
SELECT string_to_array('a,b,c', ',', 'b');
