-- Minimal relation fixture required by the vendored PostgreSQL regression
-- inputs. The rows and types are copied from PostgreSQL's test_setup.sql;
-- native regress.so helpers and unrelated schedule state are deliberately
-- absent.

CREATE TABLE FLOAT8_TBL(f1 float8);
INSERT INTO FLOAT8_TBL(f1) VALUES
  ('0.0'),
  ('-34.84'),
  ('-1004.30'),
  ('-1.2345678901234e+200'),
  ('-1.2345678901234e-200');

CREATE TABLE INT4_TBL(f1 int4);
INSERT INTO INT4_TBL(f1) VALUES
  ('   0  '),
  ('123456     '),
  ('    -123456'),
  ('2147483647'),
  ('-2147483647');

CREATE TABLE INT8_TBL(q1 int8, q2 int8);
INSERT INTO INT8_TBL VALUES
  ('  123   ','  456'),
  ('123   ','4567890123456789'),
  ('4567890123456789','123'),
  (+4567890123456789,'4567890123456789'),
  ('+4567890123456789','-4567890123456789');

CREATE TABLE TEXT_TBL (f1 text);
INSERT INTO TEXT_TBL VALUES
  ('doh!'),
  ('hi de ho neighbor');
