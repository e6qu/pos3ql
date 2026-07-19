-- no groups: whole match as a one-element array
SELECT regexp_matches('abc123', '[0-9]+');
SELECT regexp_matches('foobar', 'x');
-- capture groups
SELECT regexp_matches('abc-123', '([a-z]+)-([0-9]+)');
SELECT regexp_matches('2024-06-15', '([0-9]+)-([0-9]+)-([0-9]+)');
-- global flag: all matches
SELECT regexp_matches('a1 b2 c3', '([a-z])([0-9])', 'g');
SELECT regexp_matches('cat dog cat', '(cat)', 'g');
-- case-insensitive
SELECT regexp_matches('Hello', '(h)(ello)', 'i');
-- non-participating group -> NULL element
SELECT regexp_matches('abc', '(a)(x)?(bc)');
-- in FROM position (set-returning)
SELECT * FROM regexp_matches('a1b2c3', '([a-z])([0-9])', 'g') AS m;
SELECT m[1], m[2] FROM regexp_matches('key=value', '(\w+)=(\w+)') AS m;
-- alternation with groups
SELECT regexp_matches('foo', '^(foo|bar)$');
-- no match with g flag: zero rows
SELECT regexp_matches('abc', '([0-9]+)', 'g');
-- \d \w \s shorthand classes (also benefits ~ operator)
SELECT regexp_matches('a b  c', '(\S+)', 'g');
SELECT 'abc123' ~ '\d+', 'abc' ~ '\d', 'a_b' ~ '^\w+$';
-- overlap-free global iteration and anchors
SELECT regexp_matches('aaa', '(a)', 'g');
SELECT regexp_matches('', '(x)?', 'g');
