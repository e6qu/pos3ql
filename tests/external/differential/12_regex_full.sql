-- bounded repetition {m} {m,} {m,n}
SELECT 'aa' ~ '^a{2}$', 'a' ~ '^a{2}$', 'aaa' ~ '^a{2}$';
SELECT 'aaaa' ~ '^a{2,}$', 'a' ~ '^a{2,}$';
SELECT 'aa' ~ '^a{1,3}$', 'aaaa' ~ '^a{1,3}$';
SELECT '2024-06-15' ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}$';
SELECT 'abab' ~ '^(ab){2}$', 'ababab' ~ '^(ab){2}$';
SELECT '' ~ '^a{0,2}$', 'aa' ~ '^a{0,2}$';
SELECT regexp_replace('aaaa', 'a{2}', 'X');
SELECT regexp_replace('aaaa', 'a{2}', 'X', 'g');
SELECT regexp_matches('2024-06-15', '([0-9]{4})-([0-9]{2})-([0-9]{2})');
-- malformed bounds are errors
SELECT 'a' ~ 'a{';
SELECT 'a' ~ 'a{2,1}';
SELECT 'a' ~ 'a{300}';
-- non-greedy quantifiers
SELECT regexp_replace('aaa', 'a+?', 'X');
SELECT regexp_replace('aaa', 'a+?', 'X', 'g');
SELECT regexp_replace('<b>bold</b>', '<.*?>', '', 'g');
SELECT regexp_matches('aaa', 'a+?');
SELECT regexp_matches('<a><b>', '<(.+?)>', 'g');
SELECT regexp_substr('aaa', 'a+?'), regexp_substr('aaa', 'a+');
SELECT regexp_matches('aaaa', 'a{2,3}?');
-- backreferences in regexp_replace
SELECT regexp_replace('abc-123', '([a-z]+)-([0-9]+)', '\2-\1');
SELECT regexp_replace('john smith', '(\w+) (\w+)', '\2, \1');
SELECT regexp_replace('aa bb', '(\w+) (\w+)', '[\1][\2]');
SELECT regexp_replace('x1 y2', '(\w)(\d)', '\2\1', 'g');
SELECT regexp_replace('abc', '(a)(x)?(c)', '\1\2\3');
SELECT regexp_replace('abc', 'b', '[\&]');
-- invalid backreference number is an error
SELECT regexp_replace('abc', '(b)', 'X\2Y');
-- literal { (not followed by a digit) and edge bounds
SELECT 'a{' ~ 'a{', 'a{,5}' ~ 'a{,5}';
SELECT 'a' ~ 'a{2x}';
SELECT regexp_replace('abc', 'b', 'X\0Y');
