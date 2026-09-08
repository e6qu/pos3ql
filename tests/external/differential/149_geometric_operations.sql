-- PostgreSQL 18 planar geometric conversions, transforms, predicates,
-- distances, intersections, component subscripting, and stored-query typing.
SELECT box(circle '<(0,0),2>'), box(point '(1,0)'),
       box(polygon '((0,0),(1,1),(2,0))'),
       bound_box(box '(1,1),(0,0)', box '(4,4),(3,3)'),
       circle(box '(1,1),(0,0)'),
       circle(polygon '((0,0),(1,3),(2,0))'),
       lseg(box '(1,0),(-1,0)'), path(polygon '((0,0),(1,1),(2,0))'),
       point(lseg '[(-1,0),(1,0)]'), point(polygon '((0,0),(1,1),(2,0))');
SELECT polygon(box '(1,1),(0,0)'), polygon(circle '<(0,0),2>'),
       polygon(4, circle '<(3,0),1>'),
       diagonal(box '(1,2),(0,0)'), height(box '(1,2),(0,0)'),
       width(box '(1,2),(0,0)'), slope(point '(0,0)', point '(2,1)');
SELECT slope(point '(0,0)', point '(0,0)'),
       slope(point '(0,0)', point '(0.0000005,1)'),
       line(point '(0,0)', point '(0.0000005,1)');
SELECT polygon(1, circle '<(0,0),1>');
SELECT polygon(circle '<(0,0),0>');
SELECT point '(NaN,Infinity)', lseg '[(NaN,0),(1,-Infinity)]',
       path '[(0,NaN),(Infinity,1)]', polygon '((NaN,0),(1,1))',
       box '(NaN,1),(0,0)', line '{NaN,1,0}', circle '<(0,0),NaN>';
SELECT circle(point '(1,2)', -3), radius(circle(point '(1,2)', -3));
SELECT line '{0.0000005,0,1}';
SELECT line(point '(0,0)', point '(0.0000005,0.0000005)');

SELECT box '(1,1),(0,0)' + point '(2,0)',
       path '[(0,0),(1,1)]' + path '[(2,2),(3,3)]',
       path '((0,0),(1,1))' + path '[(2,2),(3,3)]',
       path '((0,0),(1,0),(1,1))' * point '(3,0)',
       path '((0,0),(1,0),(1,1))' / point '(2,0)';
SELECT @-@ path '[(0,0),(1,0),(1,1)]',
       @@ lseg '[(0,0),(2,2)]', # polygon '((0,0),(1,1))',
       lseg '[(0,0),(1,1)]' # lseg '[(1,0),(0,1)]',
       box '(2,2),(-1,-1)' # box '(1,1),(-2,-2)',
       point '(0,0)' ## lseg '[(2,0),(0,2)]';
SELECT circle '<(0,0),1>' <-> circle '<(5,0),1>',
       point '(0,0)' <-> line '{1,0,-3}',
       point '(0,0)' <-> box '(4,4),(2,2)',
       lseg '[(0,0),(1,0)]' <-> line '{1,0,-3}',
       polygon '((0,0),(2,0),(0,2))' <-> circle '<(8,8),1>';
SELECT point '(1.0000005,0)' <-> circle '<(0,0),1>',
       circle '<(0,0),1>' @> point '(1.0000005,0)',
       circle '<(0,0),1>' <-> circle '<(2.0000005,0),1>',
       circle '<(0,0),1>' && circle '<(2.0000005,0),1>';
-- Every documented point/object distance and same-kind distance family.
SELECT point '(0,0)' <-> point '(3,4)', point '(0,0)' <-> lseg '[(3,0),(3,4)]',
       point '(0,0)' <-> path '[(3,0),(3,4)]',
       point '(0,0)' <-> polygon '((3,0),(5,0),(5,2),(3,2))',
       point '(0,0)' <-> circle '<(5,0),2>';
SELECT box '(1,1),(0,0)' <-> box '(4,1),(3,0)',
       lseg '[(0,0),(1,0)]' <-> lseg '[(0,3),(1,3)]',
       line '{1,0,0}' <-> line '{1,0,-4}',
       path '[(0,0),(1,0)]' <-> path '[(0,2),(1,2)]',
       polygon '((0,0),(1,0),(0,1))' <-> polygon '((3,0),(4,0),(3,1))';
SELECT box '(1,1),(0,0)' <-> lseg '[(3,0),(3,1)]',
       lseg '[(3,0),(3,1)]' <-> box '(1,1),(0,0)',
       line '{1,0,0}' <-> lseg '[(4,0),(4,1)]',
       lseg '[(4,0),(4,1)]' <-> line '{1,0,0}',
       circle '<(0,0),1>' <-> polygon '((4,0),(5,0),(4,1))';

SELECT point '(1,2)' - point '(3,4)', point '(1,2)' * point '(0,1)',
       point '(1,2)' / point '(0,1)',
       box '(2,2),(0,0)' - point '(1,1)',
       path '[(0,0),(1,0)]' + point '(2,3)',
       circle '<(1,1),2>' * point '(0,2)',
       radius(circle '<(1,1),2>' * point '(0,2)');
SELECT @@ box '(2,2),(0,0)', @@ polygon '((0,0),(3,0),(0,3))',
       @-@ lseg '[(0,0),(3,4)]', length(lseg '[(0,0),(3,4)]'),
       length(path '((-1,0),(1,0))'), # path '[(0,0),(1,1),(2,2)]';

SELECT circle '<(0,0),2>' @> point '(1,1)',
       point '(1,1)' <@ circle '<(0,0),2>',
       lseg '[(0,0),(1,1)]' <@ box '(2,2),(-1,-1)',
       box '(1,1),(0,0)' && box '(2,2),(0,0)',
       circle '<(0,0),1>' << circle '<(5,0),1>',
       box '(3,3),(0,0)' <<| box '(5,5),(3,4)',
       box '(1,1),(0,0)' &<| box '(2,2),(0,0)',
       box '(1,1),(0,0)' <^ box '(2,2),(1,1)';
SELECT lseg '[(-1,0),(1,0)]' ?# box '(2,2),(-2,-2)',
       line '{1,0,0}' ?# line '{0,1,0}',
       line '{1,2,3}' ?# line '{2,4,6}',
       line '{1,2,3}' # line '{2,4,6}',
       ?- lseg '[(-1,0),(1,0)]', ?| line '{1,0,0}',
       point '(1,0)' ?- point '(0,0)', point '(0,1)' ?| point '(0,0)',
       lseg '[(0,0),(0,1)]' ?-| lseg '[(0,0),(1,0)]',
       line '{0,1,0}' ?|| line '{0,1,-2}',
       polygon '((0,0),(1,1))' ~= polygon '((1,1),(0,0))';
SELECT ishorizontal(point '(1,0)', point '(0,0)'),
       isvertical(point '(0,1)', point '(0,0)'),
       ishorizontal(lseg '[(-1,0),(1,0)]'), isvertical(line '{1,0,0}'),
       isparallel(line '{0,1,0}', line '{0,1,-2}'),
       isparallel(lseg '[(0,0),(1,1)]', lseg '[(2,0),(3,1)]'),
       isperp(line '{0,1,0}', line '{1,0,0}'),
       isperp(lseg '[(0,0),(0,1)]', lseg '[(0,0),(1,0)]');
SELECT box '(4,4),(0,0)' @> box '(3,3),(1,1)',
       path '[(0,0),(3,0)]' @> point '(2,0)',
       polygon '((0,0),(4,0),(0,4))' @> point '(1,1)',
       polygon '((0,0),(4,0),(0,4))' @> polygon '((1,1),(2,1),(1,2))',
       circle '<(0,0),4>' @> circle '<(1,0),2>';
SELECT point '(1,0)' <@ lseg '[(0,0),(2,0)]',
       point '(1,1)' <@ line '{1,-1,0}',
       point '(1,0)' <@ path '[(0,0),(2,0)]',
       point '(1,1)' <@ polygon '((0,0),(4,0),(0,4))',
       box '(3,3),(1,1)' <@ box '(4,4),(0,0)',
       lseg '[(1,1),(2,2)]' <@ box '(4,4),(0,0)',
       lseg '[(1,1),(2,2)]' <@ line '{1,-1,0}';
SELECT polygon '((0,0),(2,0),(0,2))' && polygon '((1,0),(3,0),(1,2))',
       circle '<(0,0),2>' && circle '<(3,0),2>',
       point '(0,0)' << point '(1,0)', point '(0,0)' <<| point '(0,1)',
       point '(0,0)' <^ point '(0,1)', point '(0,1)' >^ point '(0,0)';
SELECT box '(1,1),(0,0)' &< box '(2,2),(0,0)',
       box '(3,3),(0,0)' &> box '(2,2),(0,0)',
       box '(3,3),(0,0)' |&> box '(2,2),(0,0)',
       box '(2,2),(1,1)' >^ box '(1,1),(0,0)';
SELECT lseg '[(-1,0),(1,0)]' ?# lseg '[(0,-1),(0,1)]',
       lseg '[(-1,0),(1,0)]' ?# line '{1,0,0}',
       line '{1,0,0}' ?# box '(1,1),(-1,-1)',
       path '[(0,0),(2,2)]' ?# path '[(0,2),(2,0)]';
SELECT point '(1,1)' ~= point '(1,1)',
       box '(2,2),(0,0)' ~= box '(0,0),(2,2)',
       circle '<(1,1),2>' ~= circle '<(1,1),2>',
       lseg '[(0,0),(3,4)]' = lseg '[(1,1),(4,5)]',
       box '(2,2),(0,0)' < box '(3,2),(0,0)',
       circle '<(0,0),1>' < circle '<(0,0),2>';
-- Comparison families deliberately differ: paths compare point counts,
-- segments compare length except for endpoint equality, and lines compare
-- proportional coefficients. Most primitives use PostgreSQL's 1e-6 fuzz.
SELECT path '[(0,0),(100,0)]' = path '[(9,9),(8,8)]',
       path '[(0,0),(100,0)]' < path '[(0,0),(1,0),(2,0)]',
       lseg '[(0,0),(3,4)]' < lseg '[(0,0),(0,6)]',
       line '{1,2,3}' = line '{2,4,6}',
       point '(1,1)' <> point '(1.000002,1)',
       point '(1,1)' ~= point '(1.0000005,1)',
       box '(1,1),(0,0)' = box '(1.0000005,1),(0,0)';
SELECT point '(1,1)' <@ path '((0,0),(4,0),(0,4))',
       point '(1,1)' <@ path '[(0,0),(4,0),(0,4)]',
       point '(9,9)' <-> path '[(0,0)]',
       path '[(0,0)]' <-> path '[(8,8),(9,9)]';
-- These tempting signatures do not exist in PostgreSQL's operator catalog.
SELECT point '(0,0)' = point '(0,0)';
SELECT path '[(0,0)]' <> path '[(0,0)]';
SELECT polygon '((0,0),(1,0),(0,1))' = polygon '((0,0),(1,0),(0,1))';
SELECT box '(1,1),(0,0)' ?# lseg '[(0,0),(1,1)]';
SELECT point '(4,5)' ## box '(2,2),(0,0)',
       point '(4,5)' ## line '{1,0,-2}',
       lseg '[(0,0),(0,1)]' ## lseg '[(3,2),(3,4)]',
       lseg '[(0,0),(0,1)]' ## box '(3,4),(2,2)',
       line '{1,0,0}' ## lseg '[(3,2),(3,4)]';

CREATE TABLE geometric_operation_rows (
  id integer PRIMARY KEY,
  p point,
  b box,
  s lseg,
  l line,
  c circle,
  translated point GENERATED ALWAYS AS (p + point '(1,1)') STORED,
  CHECK (c @> p)
);
INSERT INTO geometric_operation_rows VALUES
  (1, point '(1,2)', box '(4,5),(0,1)', lseg '[(2,3),(6,7)]', line '{1,2,3}', circle '<(0,0),10>');
SELECT p[0], p[1], b[0], b[1], s[0], s[1], l[0], l[1], l[2], translated
  FROM geometric_operation_rows;
UPDATE geometric_operation_rows
   SET p[1] = 9, b[0] = point '(8,10)', s[1] = point '(11,12)', l[2] = 7
 WHERE id = 1
 RETURNING p, b, s, l, translated;
UPDATE geometric_operation_rows SET l[0] = 0, l[1] = 0 RETURNING l;

CREATE VIEW geometric_operation_view AS
  SELECT id, p <-> point '(0,0)' AS origin_distance,
         b @> p AS box_contains_point
    FROM geometric_operation_rows;
PREPARE geometric_operation_query(point) AS
  SELECT id, p <-> $1 AS distance FROM geometric_operation_rows;
EXECUTE geometric_operation_query(point '(1,9)');
SELECT * FROM geometric_operation_view;
SELECT pg_typeof(@@ b), pg_typeof(p <-> point '(0,0)'),
       pg_typeof(p[0]), pg_typeof(b[0]),
       pg_typeof(length(path '[(0,0),(1,0)]'))
  FROM geometric_operation_rows;
SELECT point '(1,1)' <-> '(4,5)',
       line '{1,0,-3}' <-> line '{1,0,-7}';
-- Untyped literals follow PostgreSQL's exact-match and unique-candidate
-- operator resolution, including transforms whose right operand is a point.
SELECT box '(1,1),(0,0)' + '(2,3)',
       path '[(0,0),(1,1)]' + '[(2,2),(3,3)]',
       circle '<(0,0),1>' && '<(1,0),1>',
       line '{1,0,0}' ## '[(3,2),(3,4)]',
       point '(1,1)' ~= '(1.0000005,1)';
-- No exact match and several viable geometric types: PostgreSQL rejects this.
SELECT point '(1,1)' <@ '((0,0),(4,0),(0,4))';
DROP VIEW geometric_operation_view;
DROP TABLE geometric_operation_rows;
