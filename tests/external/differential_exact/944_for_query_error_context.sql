-- A FOR loop reads its query's rows from a cursor: an error raised fetching
-- them names no SQL statement, while one raised planning the query (a
-- constant part folded, a name analyzed) does.
CREATE TABLE fe944 (id int PRIMARY KEY);
INSERT INTO fe944 VALUES (1);
CREATE FUNCTION fe944a() RETURNS int LANGUAGE plpgsql AS $$
DECLARE r record; n int := 0;
BEGIN
  FOR r IN EXECUTE 'INSERT INTO fe944 VALUES (1) RETURNING id' LOOP n := n + 1; END LOOP;
  RETURN n;
END $$;
SELECT fe944a();
CREATE FUNCTION fe944b() RETURNS int LANGUAGE plpgsql AS $$
DECLARE r record; n int := 0;
BEGIN
  FOR r IN EXECUTE 'SELECT 1 / (id - 1) FROM fe944' LOOP n := n + 1; END LOOP;
  RETURN n;
END $$;
SELECT fe944b();
CREATE FUNCTION fe944c() RETURNS int LANGUAGE plpgsql AS $$
DECLARE r record; n int := 0;
BEGIN
  FOR r IN EXECUTE 'SELECT nosuch FROM fe944' LOOP n := n + 1; END LOOP;
  RETURN n;
END $$;
SELECT fe944c();
CREATE FUNCTION fe944d() RETURNS int LANGUAGE plpgsql AS $$
DECLARE r record; n int := 0;
BEGIN
  FOR r IN SELECT 1 / (id - 1) AS x FROM fe944 LOOP n := n + 1; END LOOP;
  RETURN n;
END $$;
SELECT fe944d();
CREATE FUNCTION fe944e() RETURNS int LANGUAGE plpgsql AS $$
DECLARE r record; n int := 0;
BEGIN
  FOR r IN EXECUTE 'SELECT 1/0 FROM fe944' LOOP n := n + 1; END LOOP;
  RETURN n;
END $$;
SELECT fe944e();
CREATE FUNCTION fe944f() RETURNS int LANGUAGE plpgsql AS $$
DECLARE r record; n int := 0;
BEGIN
  FOR r IN SELECT id FROM fe944 WHERE id > 1/0 LOOP n := n + 1; END LOOP;
  RETURN n;
END $$;
SELECT fe944f();
CREATE FUNCTION fe944g() RETURNS int LANGUAGE plpgsql AS $$
DECLARE r record; n int := 0;
BEGIN
  FOR r IN INSERT INTO fe944 VALUES (1) RETURNING id LOOP n := n + 1; END LOOP;
  RETURN n;
END $$;
SELECT fe944g();
CREATE FUNCTION fe944h() RETURNS int LANGUAGE plpgsql AS $$
DECLARE r record; n int := 0;
BEGIN
  FOR r IN SELECT id FROM fe944 LOOP n := n + 1 / 0; END LOOP;
  RETURN n;
END $$;
SELECT fe944h();
DROP FUNCTION fe944a(), fe944b(), fe944c(), fe944d(), fe944e(), fe944f(), fe944g(), fe944h();
DROP TABLE fe944;
