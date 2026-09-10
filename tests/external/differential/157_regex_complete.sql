SELECT regexp_replace('abc abc abc', 'abc', 'X', 5),
       regexp_replace('abc abc abc', 'abc', 'X', 5, 2),
       regexp_replace('abc abc abc', 'abc', 'X', 5, 0, '');
SELECT regexp_replace('åβ', '', 'X', 'g');
SELECT regexp_count('abc', '', 4), regexp_count('abc', '', 5),
       regexp_instr('abc', '', 4), regexp_instr('abc', '', 5),
       regexp_substr('abc', '', 4) IS NOT NULL,
       regexp_substr('abc', '', 5) IS NULL,
       regexp_replace('abc', '', 'X', 4), regexp_replace('abc', '', 'X', 5);
SELECT regexp_count('abcabc', 'a'), regexp_count('abcabc', 'a', 2),
       regexp_count('åβ', '');
SELECT regexp_instr('abc123def456', '([0-9]+)', 1, 2, 0, '', 1),
       regexp_instr('abc123def456', '([0-9]+)', 1, 2, 1, '', 1),
       regexp_instr('abc', 'x');
SELECT regexp_substr('abc123def456', '([0-9]+)', 1, 2, '', 1),
       regexp_substr('abc', 'x') IS NULL;
SELECT regexp_match('abc-123', '([a-z]+)-([0-9]+)'),
       regexp_match('abc', 'x') IS NULL;
SELECT regexp_matches('a1 b22', '([a-z])([0-9]+)', 'g');
SELECT regexp_split_to_array('åβ', ''),
       regexp_split_to_array('a1b22c', '[0-9]+');
SELECT regexp_split_to_array('abc', 'b*'),
       regexp_split_to_array('abc', '(?=b)'),
       regexp_split_to_array('abc', '(?<=b)'),
       regexp_split_to_array('abc', '^|$');
SELECT regexp_split_to_table('a1b22c', '[0-9]+');

SELECT regexp_like('a.b', 'a.b', 'q'),
       regexp_like('ab', 'a # ignored' || chr(10) || ' b', 'x');
SELECT regexp_like(E'a\nb', '^b$', 'n'),
       regexp_like(E'a\nb', 'a.b', 'p'),
       regexp_like(E'a\nb', 'a.b', 's'),
       regexp_like(E'a\nb', '^b$', 'w');
SELECT regexp_like('abc', 'ABC', 'ic'), regexp_like('abc', 'ABC', 'ci');
SELECT regexp_like('a+b', 'a+b', 'b'), regexp_like('aaab', 'a+b', 'e');
SELECT regexp_like('A', '(?i)a', 'b'), regexp_like('(?i)a', '(?i)a', 'b'),
       regexp_like('A', '(?i)a', 'e');
SELECT regexp_like('d', '\d', 'e'), regexp_like('1', '\d', 'e'),
       regexp_like('d', '[\d]', 'b'), regexp_like('1', '[\d]', 'b'),
       regexp_like('0', '\0', 'e');
SELECT regexp_like('a+b', '***=a+b'), regexp_like('aaab', '***:a+b'),
       regexp_like('ABC', '(?i)abc'), regexp_like(E'a\nb', '(?n)^b$');
SELECT regexp_like('a b', '***=a b', 'x');

SELECT 'bbbbb' ~ '^([bc])\1*$', 'ccc' ~ '^([bc])\1*$',
       'bbc' ~ '^([bc])\1*$';
SELECT 'abc abc abc' ~ '^(\w+)( \1)+$',
       'abc abd abc' ~ '^(\w+)( \1)+$';
SELECT regexp_match('abc 123', '[[:alpha:]]+\s+[[:digit:]]+');
SELECT regexp_match('foo bar', '\mbar\M'), regexp_match('foobar', '\mbar\M');
SELECT regexp_match('foobar', 'foo(?=bar)'), regexp_match('foobaz', 'foo(?!bar)');
SELECT regexp_match('foobar', '(?<=foo)b+'), regexp_match('foobar', '(?<!foo)b+');
SELECT regexp_match('bar', '(?:foo|bar)'), regexp_match('abc', '\x61\u0062\U00000063');
SELECT regexp_match(E'\\', '\B'), regexp_match(chr(1), '\cA'),
       regexp_match('A', '\101'), regexp_match('123', '[\d]+'),
       regexp_match(E'\\', '[\B]'), regexp_match('A', '[\101]');
SELECT 'é' ~ '[[:alpha:]]', 'é' ~ '\w', 'é' ~* 'É',
       chr(160) ~ '\s';

-- PostgreSQL's upstream ARE regression boundary: quantified backreferences,
-- lookaround constraints, nested capture shapes, and non-greedy groups.
SELECT substring('asd TO foo' FROM ' TO (([a-z0-9._]+|"([^"]+|"")+")+)');
SELECT substring('a' FROM '((a))+'), substring('a' FROM '((a)+)');
SELECT regexp_matches('ab', 'a(?=b)b*');
SELECT regexp_matches('a', 'a(?=b)b*');
SELECT regexp_matches('abc', 'a(?=b)b*(?=c)c*');
SELECT regexp_matches('ab', 'a(?!b)b*');
SELECT regexp_matches('a', 'a(?!b)b*');
SELECT regexp_matches('abb', '(?<=a)b*');
SELECT regexp_matches('a', 'a(?<=a)b*');
SELECT regexp_matches('abc', 'a(?<=a)b*(?<=b)c*');
SELECT regexp_matches('ab', 'a*(?<!a)b*');
SELECT regexp_matches('b', 'a*(?<!a)b+');
SELECT regexp_matches('foobar', '(?<=oo)b+');
SELECT regexp_match('ab', '(?=(ab))a'),
       regexp_match('ab', '(?=(ab))(a)');
SELECT 'ab' ~ '(?<=^a)b', 'ab' ~ '(?<=\Aa)b',
       'a b' ~ '(?<=\ma )b',
       regexp_like(E'x\na b', '(?<=^a )b', 'n');
SELECT 'xy' ~ 'x(?=[xy])', 'xz' ~ 'x(?![xy])';
SELECT 'xyy' ~ '(?<=[xy])yy+', 'zyy' ~ '(?<![xy])yy+';
SELECT 'Programmer' ~ '(\w).*?\1';
SELECT regexp_matches('Programmer', '(\w)(.*?\1)', 'g');
SELECT regexp_matches(
    'foo/bar/baz',
    '^([^/]+?)(?:/([^/]+?))(?:/([^/]+?))?$'
);
SELECT 'a' ~ '($|^)*', 'a' ~ '(^)+^', 'a' ~ '$($$)+',
       'a' ~ '($^)+', 'a' ~ '(^$)*';
SELECT 'aa bb cc' ~ '(^(?!aa))+',
       'aa x' ~ '(^(?!aa)(?!bb)(?!cc))+',
       'bb x' ~ '(^(?!aa)(?!bb)(?!cc))+',
       'cc x' ~ '(^(?!aa)(?!bb)(?!cc))+',
       'dd x' ~ '(^(?!aa)(?!bb)(?!cc))+';
SELECT 'a' ~ '((((((a)*)*)*)*)*)*',
       'a' ~ '((((((a+|)+|)+|)+|)+|)+|)';
SELECT 'x' ~ 'abcd(\m)+xyz',
       'x' ~ 'xyz(\Y\Y)+',
       'x' ~ 'x|(?:\M)+';
SELECT regexp_matches('llmmmfff', '^(l*)(.*)(f*)$');
SELECT regexp_matches('llmmmfff', '^(l*){1,1}(.*)(f*)$');
SELECT regexp_matches('llmmmfff', '^(l*){1,1}?(.*)(f*)$');
SELECT regexp_matches('llmmmfff', '^(l*){1,1}?(.*){1,1}?(f*)$');
SELECT regexp_matches('llmmmfff', '^(l*?)(.*)(f*)$');
SELECT regexp_matches('llmmmfff', '^(l*?){1,1}(.*)(f*)$');
SELECT regexp_matches('llmmmfff', '^(l*?){1,1}?(.*)(f*)$');
SELECT regexp_matches('llmmmfff', '^(l*?){1,1}?(.*){1,1}?(f*)$');
SELECT 'a' ~ '$()|^\1', 'a' ~ '.. ()|\1',
       'a' ~ '()*\1', 'a' ~ '()+\1';
SELECT 'xxx' ~ '(.){0}(\1)',
       'xxx' ~ '((.)){0}(\2)',
       'xyz' ~ '((.)){0}(\2){0}';
SELECT 'abcdef' ~ '^(.)\1|\1.',
       'abadef' ~ '^((.)\2|..)\2';
SELECT regexp_match('xy', '.|...'), regexp_match('xyz', '.|...'),
       regexp_match('xy', '.*'), regexp_match('fooba', '(?:..)*');
SELECT regexp_match('xyz', repeat('.', 260)),
       regexp_match('foo', '(?:.|){99}');

SELECT similar_to_escape('a%b'), similar_to_escape('a#%b', '#'),
       similar_to_escape('a%b', '');
SELECT textregexeq('abc', 'b'), textregexne('abc', 'x'),
       texticregexeq('ABC', 'abc'), texticregexne('ABC', 'abc');
SELECT nameregexeq('abc'::name, 'b'), nameregexne('abc'::name, 'x'),
       nameicregexeq('ABC'::name, 'abc'), nameicregexne('ABC'::name, 'abc');
SELECT regexp_instr(subexpr => 1, flags => '', endoption => 0, "N" => 2,
                    start => 1, pattern => '([0-9]+)', string => 'a1b22'),
       pg_catalog.regexp_count(flags => 'i', start => 1,
                               pattern => 'a', string => 'Aa');
SELECT regexp_replace(flags => 'g', replacement => 'X',
                      pattern => 'a', string => 'aba');
SELECT regexp_matches(flags => 'g', pattern => '([0-9]+)', string => 'a1b22');
SELECT * FROM pg_catalog.regexp_split_to_table(
    flags => '', pattern => '[0-9]+', string => 'a1b22');

SELECT oid, proname, pronamespace, proowner, prorettype,
       proargtypes::text, pronargs, prolang, prokind,
       provolatile, proparallel, proisstrict, proretset,
       prosecdef, proleakproof, procost, prorows, prosupport,
       pronargdefaults, provariadic, prosrc, proargnames::text
FROM pg_proc
WHERE oid IN (
    79, 1024, 1238, 1239, 1240, 1241, 1252, 1254, 1256, 1364,
    1818, 1820, 1821, 1823, 1824, 1826, 1827, 1829,
    1986, 1987, 2284, 2285, 2763, 2764, 2765, 2766,
    2767, 2768, 3396, 3397, 6251, 6252, 6253, 6254,
    6255, 6256, 6257, 6258, 6259, 6260, 6261, 6262,
    6263, 6264, 6265, 6266, 6267, 6268, 6269)
ORDER BY oid;
SELECT oid, oprname, oprnamespace, oprowner, oprkind,
       oprcanmerge, oprcanhash, oprleft, oprright, oprresult,
       oprcode::regproc, oprcom, oprnegate,
       oprrest::regproc, oprjoin::regproc
FROM pg_operator
WHERE oid IN (639, 640, 641, 642, 1226, 1227, 1228, 1229)
ORDER BY oid;

CREATE TABLE regex_complete (
    id integer PRIMARY KEY,
    source text,
    first_number text GENERATED ALWAYS AS
        (regexp_substr(source, '([0-9]+)', 1, 1, '', 1)) STORED,
    CONSTRAINT regex_complete_shape CHECK (source ~ '^[[:alpha:]]+[0-9]+$')
);
CREATE INDEX regex_complete_match ON regex_complete
    ((regexp_match(source, '([[:alpha:]]+)([0-9]+)')));
CREATE VIEW regex_complete_view AS
SELECT id, first_number, regexp_count(source, '[0-9]') AS digit_count
FROM regex_complete;
INSERT INTO regex_complete (id, source) VALUES (1, 'abc123'), (2, 'z9');
SELECT * FROM regex_complete_view ORDER BY id;
SELECT id FROM regex_complete
WHERE regexp_match(source, '([[:alpha:]]+)([0-9]+)') = ARRAY['abc', '123'];
PREPARE regex_complete_query(text, text) AS
SELECT regexp_match($1, $2), regexp_instr($1, $2, 1, 1, 1, '', 0);
EXECUTE regex_complete_query('xy42', '([a-z]+)([0-9]+)');
DEALLOCATE regex_complete_query;

SELECT regexp_count('abc', 'a', 0);
SELECT regexp_instr('abc', 'a', 1, 0);
SELECT regexp_instr('abc', 'a', 1, 1, 2);
SELECT regexp_substr('abc', 'a', 1, 0);
SELECT regexp_replace('abc', 'a', 'x', 0);
SELECT regexp_replace('abc', 'a', 'x', 1, -1);
SELECT regexp_like('abc', 'a', 'g');
SELECT regexp_match('abc', 'a', 'g');
SELECT regexp_split_to_array('abc', 'a', 'g');
SELECT regexp_split_to_table('abc', 'a', 'g');
SELECT regexp_like('abc', '[[:bogus:]]');
SELECT regexp_like('abc', '[z-a]');
SELECT regexp_like('abc', '[a-[:digit:]]');
SELECT regexp_like('abc', '[[:digit:]-a]');
SELECT regexp_like('abc', '\x7fffffff');
SELECT regexp_like('abc', '(?z)a');
SELECT regexp_like('a b', 'a b', 'qx');
SELECT regexp_like('a', '*a');
SELECT regexp_like('a', 'a**');
SELECT regexp_like('a', '{1}a');
SELECT regexp_like('a', '^*a');
SELECT regexp_like('abc', '\q');
SELECT regexp_like('abc', '[\q]');
SELECT regexp_like('abc', '\1');
SELECT regexp_like('abc', '\1', 'e');
SELECT regexp_instr(string => 'a1', pattern => '1', start => 1, n => 1);
SELECT regexp_count("STRING" => 'a', pattern => 'a');
SELECT 'xyz' ~ 'x(\w)(?=\1)';
SELECT 'xyz' ~ 'x(\w)(?=(\1))';
SELECT 'a' ~ '(?=(a))\1';
