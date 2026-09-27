-- Geometric values use statement memory rather than the former 128-point
-- implementation envelope.
SELECT npoints(polygon(300, circle '<(0,0),1>')),
       npoints(path(polygon(300, circle '<(0,0),1>')) + point '(1,2)'),
       npoints(popen(path(polygon(150, circle '<(0,0),1>'))) +
               popen(path(polygon(150, circle '<(2,2),1>')))),
       isclosed(pclose(path(polygon(300, circle '<(0,0),1>')))),
       length(path(polygon(300, circle '<(0,0),1>'))) > 0,
       octet_length(polygon(300, circle '<(0,0),1>')::text) > 2048;

CREATE TABLE wide_geometry_values (
  id integer PRIMARY KEY,
  shape polygon,
  route path
);
INSERT INTO wide_geometry_values VALUES (
  1,
  polygon(300, circle '<(0,0),1>'),
  path(polygon(300, circle '<(0,0),1>'))
);
CREATE INDEX wide_geometry_values_shape
  ON wide_geometry_values USING gist (shape);
SELECT id, npoints(shape), npoints(route), length(route) > 0
  FROM wide_geometry_values
 WHERE shape && polygon(300, circle '<(0,0),1>');
DROP TABLE wide_geometry_values;
