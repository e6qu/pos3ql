-- PostgreSQL 18: SPI_cursor_open_internal rejects zero or multiple commands.
-- https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/executor/spi.c
CREATE SEQUENCE cursor_query_calls;
CREATE FUNCTION cursor_query_count(q text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE c refcursor;
BEGIN
  OPEN c FOR EXECUTE q;
  CLOSE c;
  RETURN;
END $$;
SELECT cursor_query_count('SELECT nextval(''cursor_query_calls''); SELECT 2');
SELECT last_value, is_called FROM cursor_query_calls;
SELECT cursor_query_count('');
DROP FUNCTION cursor_query_count(text);
DROP SEQUENCE cursor_query_calls;
