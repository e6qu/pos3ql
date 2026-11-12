-- An assignment error for a column that `t.*` expanded is positioned at `t`.
CREATE TABLE qs113_target (a int, b text);
CREATE TABLE qs113_source (x timestamptz, y text);
INSERT INTO qs113_target SELECT s.* FROM qs113_source s;
INSERT INTO qs113_target SELECT qs113_source.* FROM qs113_source;
INSERT INTO qs113_target (b, a) SELECT s.* FROM qs113_source s;
INSERT INTO qs113_target SELECT s.y, s.x FROM qs113_source s;
