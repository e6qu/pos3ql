-- LIKE's operator spellings (~~, !~~, ~~*, !~~*), quantified pattern matches
-- over arrays and subqueries, and pattern matches between non-string types.
select 'abc' ~~ 'a%', 'abc' !~~ 'a%', 'ABC' ~~* 'a%', 'ABC' !~~* 'a%';
select 'abc' ~~ any (array['x%','a%']), 'abc' ~~* all (array['A%']), 'abc' !~~ any (array['a%']), 'abc' !~~ all (array['x%']);
select 'abc' ~~ any (select 'a%'), 'abc' !~~* all (select 'X%');
select 'a_c' ~~ 'a\_c', 'abc' ~~ 'a\_c';
select 'abc' ~~ 'a%' = true;
select 1 ~~ 'a';
select 'abc' like any (select 'a%'), 'abc' not like any (select 'a%');
select 1 like 'a';
select 1 not ilike 'a';
select 'a' ~ 1;
select 2 !~* 'x';
select 1.5 like 'a';
