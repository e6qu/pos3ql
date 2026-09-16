-- PostgreSQL 18.6 is the oracle for geometric navigation's exact rechecks.
SELECT NULL::point ~= '(0,0)'::point, '(0,0)'::point ~= NULL::point,
 NULL::point ~= NULL::point, NULL::point <@ '((0,0),(0,0))'::box,
 '((0,0),(0,0))'::box @> NULL::point, NULL::point <-> '(0,0)'::point;
PREPARE navigation_nullable(point,box) AS SELECT $1 ~= '(0,0)'::point,$1 <@ $2;
EXECUTE navigation_nullable(NULL,'((0,0),(0,0))');
DEALLOCATE navigation_nullable;
CREATE TABLE navigation_points(id int PRIMARY KEY, shape point, payload text);
CREATE TABLE navigation_boxes(id int PRIMARY KEY, shape box, payload text);
CREATE TABLE navigation_polygons(id int PRIMARY KEY, shape polygon, payload text);
CREATE TABLE navigation_circles(id int PRIMARY KEY, shape circle, payload text);
-- PostgreSQL 18.6 native GiST can miss finite keys when NaN is present;
-- quad-point construction can fail. Qualify non-finite SQL without those
-- native-tree paths; Rust cold tests qualify our tree with the same operands.
CREATE TABLE navigation_nonfinite(id int,shape point);
INSERT INTO navigation_nonfinite VALUES(1,'(0,0)'),(7,'(Infinity,0)'),(8,'(NaN,0)'),(9,NULL);
SELECT id FROM navigation_nonfinite WHERE shape ~= '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_nonfinite WHERE shape << '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_nonfinite WHERE shape >> '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_nonfinite WHERE shape <<| '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_nonfinite WHERE shape |>> '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_nonfinite WHERE shape <@ '((0,0),(0,0))'::box ORDER BY id;
INSERT INTO navigation_points VALUES
 (1,'(0,0)', 'origin'), (2,'(-0.0000005,0)','near-left'),
 (3,'(0.0000005,0)','near-right'), (4,'(0,-0.0000005)','near-below'),
 (5,'(0,0.0000005)','near-above'), (6,'(-0,-0)','signed-zero'),
 (9,NULL,'null');
INSERT INTO navigation_points SELECT value+20,point(value*100,value*100),repeat('p',192)
 FROM generate_series(1,500) source(value);
INSERT INTO navigation_boxes VALUES
 (1,'(1,1),(0,0)','positive'), (2,'(0,0),(-1,-1)','negative'),
 (3,'(0.0000005,0.0000005),(-0.0000005,-0.0000005)','fuzzy'), (4,NULL,'null');
INSERT INTO navigation_boxes SELECT value+20,
 box(point(value*100-1,value*100-1),point(value*100+1,value*100+1)),repeat('b',192)
 FROM generate_series(1,500) source(value);
INSERT INTO navigation_polygons VALUES
 (1,'((0,0),(1,0),(0,1))','positive'),
 (2,'((-1,-1),(0,-1),(-1,0))','negative'),
 (3,'((-0.0000005,-0.0000005),(0.0000005,-0.0000005),(0,0.0000005))','fuzzy'),
 (4,NULL,'null');
INSERT INTO navigation_polygons SELECT value+20,
 polygon(box(point(value*100-1,value*100-1),point(value*100+1,value*100+1))),repeat('g',192)
 FROM generate_series(1,500) source(value);
INSERT INTO navigation_circles VALUES
 (1,'<(0,0),1>','origin'), (2,'<(1,0),1>','tangent'),
 (3,'<(0,0),0>','zero-radius'), (4,NULL,'null');
INSERT INTO navigation_circles SELECT value+20,circle(point(value*100,value*100),2),repeat('c',192)
 FROM generate_series(1,500) source(value);
CREATE INDEX navigation_points_shape ON navigation_points USING gist(shape) INCLUDE(id,payload);
CREATE INDEX navigation_boxes_shape ON navigation_boxes USING gist(shape) INCLUDE(id,payload);
CREATE INDEX navigation_polygons_shape ON navigation_polygons USING gist(shape) INCLUDE(id,payload);
CREATE INDEX navigation_circles_shape ON navigation_circles USING gist(shape) INCLUDE(id,payload);
ANALYZE navigation_points;
ANALYZE navigation_boxes;
ANALYZE navigation_polygons;
ANALYZE navigation_circles;

SELECT id FROM navigation_points WHERE shape ~= '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape << '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape >> '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape <<| '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape |>> '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape <^ '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape >^ '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape <@ '((0,0),(0,0))'::box ORDER BY id;
SELECT id,length(payload) FROM navigation_points WHERE shape <@ '((11999,11999),(12001,12001))'::box ORDER BY id;
SELECT id FROM navigation_boxes WHERE shape && '((0,0),(0,0))'::box ORDER BY id;
SELECT id FROM navigation_boxes WHERE shape @> '((0,0),(0,0))'::box ORDER BY id;
SELECT id FROM navigation_boxes WHERE shape <@ '((1,1),(-1,-1))'::box ORDER BY id;
SELECT id FROM navigation_boxes WHERE shape &< '((0,0),(0,0))'::box ORDER BY id;
SELECT id FROM navigation_boxes WHERE shape &> '((0,0),(0,0))'::box ORDER BY id;
SELECT id FROM navigation_boxes WHERE shape &<| '((0,0),(0,0))'::box ORDER BY id;
SELECT id FROM navigation_boxes WHERE shape |&> '((0,0),(0,0))'::box ORDER BY id;
SELECT id,length(payload) FROM navigation_boxes WHERE shape ~= '((12001,12001),(11999,11999))'::box;
SELECT id FROM navigation_polygons WHERE shape @> '((0,0),(0.0000001,0),(0,0.0000001))'::polygon ORDER BY id;
SELECT id FROM navigation_polygons WHERE shape && '((0,0),(0.0000001,0),(0,0.0000001))'::polygon ORDER BY id;
SELECT id FROM navigation_polygons WHERE shape <@ '((-1,-1),(1,-1),(1,1),(-1,1))'::polygon ORDER BY id;
SELECT id,length(payload) FROM navigation_polygons
 WHERE shape ~= '((11999,11999),(12001,11999),(12001,12001),(11999,12001))'::polygon;
SELECT id FROM navigation_circles WHERE shape && '<(0,0),0>'::circle ORDER BY id;
SELECT id FROM navigation_circles WHERE shape @> '<(0,0),0>'::circle ORDER BY id;
SELECT id FROM navigation_circles WHERE shape <@ '<(0,0),1>'::circle ORDER BY id;
SELECT id,length(payload) FROM navigation_circles WHERE shape ~= '<(12000,12000),2>'::circle;
SELECT id FROM navigation_points WHERE shape <@ '((0,0),(0,0))'::box ORDER BY shape <-> '(0,0)'::point,id;
SELECT id FROM navigation_circles WHERE shape <@ '<(0,0),2>'::circle ORDER BY shape <-> '(0,0)'::point,id;

BEGIN;
SAVEPOINT before_spatial_mutation;
UPDATE navigation_points SET shape='(0,0)',payload='pending' WHERE id=140;
SELECT id,payload FROM navigation_points WHERE shape ~= '(0,0)'::point ORDER BY id;
ROLLBACK TO before_spatial_mutation;
SELECT id,length(payload) FROM navigation_points WHERE shape ~= '(12000,12000)'::point;
COMMIT;
UPDATE navigation_points SET shape='(0,0)',payload='committed' WHERE id=140;
DELETE FROM navigation_points WHERE id=1;
SELECT id,payload FROM navigation_points WHERE shape ~= '(0,0)'::point ORDER BY id;
REINDEX INDEX navigation_points_shape;
SELECT id,payload FROM navigation_points WHERE shape ~= '(0,0)'::point ORDER BY id;

DROP INDEX navigation_points_shape;
DROP INDEX navigation_boxes_shape;
DROP INDEX navigation_polygons_shape;
CREATE INDEX navigation_points_shape ON navigation_points USING spgist(shape quad_point_ops) INCLUDE(id,payload);
CREATE INDEX navigation_boxes_shape ON navigation_boxes USING spgist(shape) INCLUDE(id,payload);
CREATE INDEX navigation_polygons_shape ON navigation_polygons USING spgist(shape) INCLUDE(id,payload);
SELECT id,payload FROM navigation_points WHERE shape ~= '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape << '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape >> '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape <<| '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape |>> '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape <@ '((0,0),(0,0))'::box ORDER BY id;
SELECT id FROM navigation_boxes WHERE shape && '((0,0),(0,0))'::box ORDER BY id;
SELECT id FROM navigation_boxes WHERE shape &< '((0,0),(0,0))'::box ORDER BY id;
SELECT id FROM navigation_boxes WHERE shape &> '((0,0),(0,0))'::box ORDER BY id;
SELECT id FROM navigation_polygons WHERE shape @> '((0,0),(0.0000001,0),(0,0.0000001))'::polygon ORDER BY id;
SELECT id,length(payload) FROM navigation_polygons
 WHERE shape ~= '((11999,11999),(12001,11999),(12001,12001),(11999,12001))'::polygon;
DROP INDEX navigation_points_shape;
CREATE INDEX navigation_points_shape ON navigation_points USING spgist(shape kd_point_ops) INCLUDE(id,payload);
SELECT id,payload FROM navigation_points WHERE shape ~= '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape << '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape >> '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape <<| '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape |>> '(0,0)'::point ORDER BY id;
SELECT id FROM navigation_points WHERE shape <@ '((0,0),(0,0))'::box ORDER BY id;
SELECT id FROM navigation_points WHERE shape <@ '((0,0),(0,0))'::box ORDER BY shape <-> '(0,0)'::point,id;

DROP TABLE navigation_points;
DROP TABLE navigation_boxes;
DROP TABLE navigation_polygons;
DROP TABLE navigation_circles;
DROP TABLE navigation_nonfinite;
