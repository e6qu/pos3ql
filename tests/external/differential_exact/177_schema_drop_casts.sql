-- DROP SCHEMA ... CASCADE drops the casts on its enums and domains, and a
-- cast on a domain warns with SQLSTATE 42809.
CREATE SCHEMA boxed;
CREATE TYPE boxed.tone AS ENUM ('low', 'high');
CREATE DOMAIN boxed.dd AS int;
CREATE CAST (boxed.tone AS text) WITH INOUT;
CREATE FUNCTION public.ddf(boxed.dd) RETURNS text LANGUAGE sql AS 'SELECT ''x''';
CREATE CAST (boxed.dd AS text) WITH FUNCTION public.ddf(boxed.dd);
DROP SCHEMA boxed CASCADE;
SELECT count(*) FROM pg_cast WHERE oid >= 16384;
SELECT count(*) FROM pg_proc WHERE proname = 'ddf';
