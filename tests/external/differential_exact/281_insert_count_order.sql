-- An INSERT compares its target and expression counts before converting any
-- value, and positions the first unmatched target or expression.
CREATE TABLE t281 (a int[], b int, c text);
INSERT INTO t281 (a, b) SELECT 1;
INSERT INTO t281 (a, b) SELECT '{1}', 2, 3;
INSERT INTO t281 SELECT '{1}', 2, 'x', 4;
INSERT INTO t281 (b) SELECT 1, 2 FROM t281;
INSERT INTO t281 (a[1], b) SELECT 1, 2, 3;
INSERT INTO t281 (b, c) VALUES (1);
INSERT INTO t281 (b) VALUES (1, 2);
INSERT INTO t281 (a, b) SELECT 'x', 2;
INSERT INTO t281 (b, c) SELECT 1, 'y';
SELECT * FROM t281;
