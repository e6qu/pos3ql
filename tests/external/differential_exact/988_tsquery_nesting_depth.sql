-- A tsquery nests as deep as PostgreSQL's max_stack_depth lets its input
-- (makepol) and output (infix) recurse. The exact depths depend on the stack
-- each level uses in a given build, so these queries stay clear of them;
-- makepol holds at most 32 operators pending at one nesting level. Combining
-- queries groups each operand as the node it becomes, and output
-- parenthesizes a phrase that is a phrase's right operand.
SELECT numnode(to_tsquery(string_agg('w' || i, ' & '))) FROM generate_series(1, 65) i;
SELECT numnode(to_tsquery(string_agg('w' || i, ' & '))), length(to_tsquery(string_agg('w' || i, ' & '))::text) FROM generate_series(1, 1000) i;
SELECT numnode((repeat('(', 1000) || 'a' || repeat(')', 1000))::tsquery);
SELECT numnode((repeat('(', 70) || 'a & b' || repeat(')', 70))::tsquery);
SELECT numnode((repeat('(', 5000) || 'a' || repeat(')', 5000))::tsquery);
SELECT numnode((repeat('(', 30000) || 'a' || repeat(')', 30000))::tsquery);
SELECT length(string_agg('w' || i, ' & ')::tsquery::text) FROM generate_series(1, 10000) i;
SELECT length(string_agg('w' || i, ' & ')::tsquery::text) FROM generate_series(1, 40000) i;
SELECT numnode((repeat('!', 31) || 'a')::tsquery), numnode((repeat('!', 32) || 'a')::tsquery);
SELECT numnode((repeat('!', 33) || 'a')::tsquery);
SELECT numnode(('a & ' || repeat('!', 31) || 'b')::tsquery);
SELECT numnode(('a & ' || repeat('!', 32) || 'b')::tsquery);
SELECT numnode(('(' || repeat('!', 30) || 'a) & ' || repeat('!', 31) || 'b')::tsquery);
SELECT to_tsvector('w1 w2') @@ to_tsquery(string_agg('w' || i, ' | ')) FROM generate_series(1, 1000) i;
SELECT numnode(to_tsquery(string_agg('(w' || i, ' & ') || repeat(')', 100))) FROM generate_series(1, 100) i;
SELECT numnode(ts_rewrite(to_tsquery(string_agg('w' || i, ' & ')), 'w5'::tsquery, 'z'::tsquery)) FROM generate_series(1, 1000) i;
SELECT length(querytree(to_tsquery(string_agg('w' || i, ' & ')))) FROM generate_series(1, 1000) i;
SELECT numnode(phraseto_tsquery(string_agg('w' || i, ' '))), numnode(to_tsquery(string_agg('w' || i, ' <-> '))) FROM generate_series(1, 1000) i;
SELECT to_tsquery(string_agg('w' || i, ' & ')) = to_tsquery(string_agg('w' || i, ' & ')) FROM generate_series(1, 1000) i;
SELECT count(DISTINCT q) FROM (SELECT to_tsquery(string_agg('w' || i, ' & ')) q FROM generate_series(1, 1000) i UNION ALL SELECT to_tsquery(string_agg('w' || i, ' & ')) FROM generate_series(1, 1000) i) s;
SELECT numnode(to_tsquery(string_agg('w' || i, ' & ')) && to_tsquery(string_agg('w' || i, ' | '))), numnode(!! to_tsquery(string_agg('w' || i, ' & '))) FROM generate_series(1, 1000) i;
SELECT length(tsquery_phrase(to_tsquery(string_agg('w' || i, ' & ')), 'x'::tsquery)::text) FROM generate_series(1, 1000) i;
SELECT numnode(plainto_tsquery(string_agg('w' || i, ' '))), numnode(websearch_to_tsquery(string_agg('w' || i, ' or '))) FROM generate_series(1, 1000) i;
SELECT 'a & b'::tsquery && 'c & d'::tsquery, 'a | b'::tsquery && 'c'::tsquery, 'a'::tsquery || 'b & c'::tsquery, 'a | b'::tsquery || 'c | d'::tsquery;
SELECT 'a <-> b'::tsquery <-> 'c <-> d'::tsquery, tsquery_phrase('a | b'::tsquery, 'c'::tsquery, 3), 'a & b'::tsquery <-> 'c'::tsquery;
SELECT !! 'a & b'::tsquery, 'a & (b & c)'::tsquery, 'a <-> (b <-> c)'::tsquery, 'a | (b | c)'::tsquery, '!!a'::tsquery, '(a <-> b) <-> c'::tsquery;
SELECT websearch_to_tsquery('english', 'cat or -dog "big fat" or'), websearch_to_tsquery('english', 'or cat dog or -rat');
