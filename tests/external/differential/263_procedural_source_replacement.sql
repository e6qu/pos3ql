CREATE FUNCTION owned_source_definition() RETURNS text LANGUAGE plpgsql AS $original$
DECLARE result text := 'original';
BEGIN
    EXECUTE $replacement$
        CREATE OR REPLACE FUNCTION owned_source_definition() RETURNS text
        LANGUAGE plpgsql AS 'BEGIN RETURN ''replacement''; END';
    $replacement$;
    RETURN result;
END
$original$;
SELECT owned_source_definition();
SELECT owned_source_definition();
DROP FUNCTION owned_source_definition();
