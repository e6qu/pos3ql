-- PostgreSQL 18 SQL/XML types, constructors, XPath, XMLTABLE, XMLAGG, and
-- their shared query, DML, procedural, cursor, COPY, and catalog boundaries.
SELECT oid, typname, typtype, typcategory, typelem, typarray
  FROM pg_type WHERE oid IN (142, 143) ORDER BY oid;
SELECT oid, proname, prorettype, proargtypes, pronargdefaults,
       proretset, provolatile, proisstrict, prokind
  FROM pg_proc
 WHERE oid IN (2895, 2900, 2901, 2931, 2932, 3049, 3050, 3051, 3052, 3053)
 ORDER BY oid;
SELECT aggfnoid, aggkind, aggnumdirectargs, aggtransfn,
       aggtranstype, agginitval
  FROM pg_aggregate WHERE aggfnoid = 'xmlagg'::regproc;

SET xmloption = content;
SHOW xmloption;
SELECT pg_typeof('<a/>'::xml), '<a/>'::xml::text,
       ARRAY['<a/>'::xml, NULL, '<b>two</b>'::xml]::text;
SELECT xmlparse(content 'before<a/>after'),
       xmlparse(document '<?xml version="1.0"?><root/>'),
       xmlserialize(content ('<a/><b/>'::xml) AS varchar(20));
SELECT xmlelement(name item,
         xmlattributes(7 AS id, 'x&y' AS label, NULL AS omitted),
         'before <', '<child/>'::xml),
       xmlforest(1 AS first, NULL AS omitted, 'x&y' AS third);
SELECT xmlconcat('<?xml version="1.0"?><a/>'::xml, NULL, '<b/>'::xml),
       xmlconcat2('<left/>'::xml, NULL),
       xmlcomment('safe < text'),
       xmlpi(name target, 'instruction < text'),
       xmlroot('<root/>'::xml, version '1.1', standalone no);
SELECT xml_is_well_formed('<a/><b/>'),
       xml_is_well_formed_document('<a/><b/>'),
       xml_is_well_formed_content('<a/><b/>'),
       xml_is_well_formed_document('<?xml version junk="1.0"?><a/>'),
       xml_is_well_formed_document(
         '<!DOCTYPE a [<!ENTITY declared "ok">]><a>&declared;</a>'),
       xml_is_well_formed_document(
         '<!DOCTYPE a [<!ENTITY declared "ok">]><a>&missing;</a>');

SELECT xmlexists('/rows/row[@kind="keep"]' PASSING BY REF
         ('<rows><row kind="skip"/><row kind="keep"/></rows>'::xml)),
       xpath_exists('//row[@kind="missing"]',
         '<rows><row kind="keep"/></rows>'::xml),
       xpath('/rows/row[2]/@kind',
         '<rows><row kind="first"/><row kind="second"/></rows>'::xml)::text;
SELECT xpath('count(/rows/row)', '<rows><row/><row/></rows>'::xml)::text,
       xpath('boolean(/rows/missing)', '<rows/>'::xml)::text,
       xpath('string(/rows/row)', '<rows><row>x&amp;y</row></rows>'::xml)::text,
       xpath('number(/rows/row)', '<rows><row>12.5</row></rows>'::xml)::text;
SELECT xpath('/q:rows/q:row/@id',
         '<p:rows xmlns:p="urn:items"><p:row id="7"/></p:rows>'::xml,
         ARRAY[ARRAY['q', 'urn:items']])::text,
       xpath_exists('/q:rows/q:missing',
         '<p:rows xmlns:p="urn:items"><p:row/></p:rows>'::xml,
         ARRAY[ARRAY['q', 'urn:items']]);

SELECT * FROM XMLTABLE(
  '/rows/row' PASSING
    ('<rows><row id="1"><name>A</name><tag>x</tag><tag>y</tag></row><row id="2"/></rows>'::xml)
  COLUMNS ord FOR ORDINALITY,
          id integer PATH '@id',
          name text PATH 'name' DEFAULT 'missing' NOT NULL,
          tags xml PATH 'tag');
SELECT * FROM XMLTABLE(
  XMLNAMESPACES('urn:items' AS q),
  '/q:rows/q:row' PASSING
    ('<p:rows xmlns:p="urn:items"><p:row id="7"><p:name>A</p:name></p:row></p:rows>'::xml)
  COLUMNS id integer PATH '@id', name text PATH 'q:name');
SELECT xmlagg(fragment ORDER BY ordering)
  FROM (VALUES (2, '<b/>'::xml), (1, '<a/>'::xml), (3, NULL::xml))
       AS source(ordering, fragment);

CREATE TABLE sql_xml_documents (
  id integer PRIMARY KEY,
  document xml NOT NULL CHECK (xml_is_well_formed_document(document::text)),
  fragments xml[],
  rendered text GENERATED ALWAYS AS (document::text) STORED
);
INSERT INTO sql_xml_documents(id, document, fragments) VALUES
  (1, '<item name="alpha"><value>10</value></item>',
      ARRAY['<a/>'::xml, NULL, '<b/>'::xml]);
CREATE VIEW sql_xml_view AS
  SELECT id, xpath_exists('/item/value', document) AS has_value,
         XMLSERIALIZE(DOCUMENT document AS text) AS rendered
    FROM sql_xml_documents;
CREATE MATERIALIZED VIEW sql_xml_materialized AS
  SELECT id, xpath('/item/@name', document)::text AS names
    FROM sql_xml_documents;
SELECT * FROM sql_xml_view;
SELECT * FROM sql_xml_materialized;

CREATE TABLE sql_xml_dml(id integer PRIMARY KEY, name text, payload xml);
INSERT INTO sql_xml_dml
  SELECT id, name, payload FROM XMLTABLE(
    '/rows/row' PASSING
      ('<rows><row id="1"><name>one</name><payload><old/></payload></row><row id="2"><name>two</name><payload><new/></payload></row></rows>'::xml)
    COLUMNS id integer PATH '@id', name text PATH 'name', payload xml PATH 'payload/*');
UPDATE sql_xml_dml AS target SET name = source.name
  FROM XMLTABLE('/rows/row' PASSING
         ('<rows><row id="2"><name>TWO</name></row></rows>'::xml)
       COLUMNS id integer PATH '@id', name text PATH 'name') AS source
 WHERE target.id = source.id;
DELETE FROM sql_xml_dml AS target USING
  XMLTABLE('/rows/row' PASSING ('<rows><row id="1"/></rows>'::xml)
    COLUMNS id integer PATH '@id') AS source
 WHERE target.id = source.id;
MERGE INTO sql_xml_dml AS target USING
  XMLTABLE('/rows/row' PASSING
      ('<rows><row id="2"><name>second</name></row><row id="3"><name>three</name></row></rows>'::xml)
    COLUMNS id integer PATH '@id', name text PATH 'name') AS source
  ON target.id = source.id
  WHEN MATCHED THEN UPDATE SET name = source.name
  WHEN NOT MATCHED THEN INSERT (id, name, payload)
    VALUES (source.id, source.name, '<inserted/>'::xml);
SELECT id, name, payload::text FROM sql_xml_dml ORDER BY id;

CREATE TABLE sql_xml_ctas AS
  SELECT * FROM XMLTABLE('/rows/row' PASSING
      ('<rows><row id="9"><name>ctas</name></row></rows>'::xml)
    COLUMNS id integer PATH '@id', name text PATH 'name');
SELECT * FROM sql_xml_ctas;
CREATE FUNCTION sql_xml_first_name(input xml) RETURNS text LANGUAGE plpgsql AS $$
DECLARE found text;
BEGIN
  SELECT name INTO found
    FROM XMLTABLE('/item' PASSING input COLUMNS name text PATH '@name');
  RETURN found;
END $$;
SELECT sql_xml_first_name(document) FROM sql_xml_documents;

BEGIN;
DECLARE sql_xml_cursor CURSOR FOR
  SELECT * FROM XMLTABLE('/rows/row' PASSING
      ('<rows><row id="4"><name>cursor</name></row></rows>'::xml)
    COLUMNS id integer PATH '@id', name text PATH 'name');
FETCH ALL FROM sql_xml_cursor;
COMMIT;
PREPARE sql_xml_prepared(xml) AS
  SELECT xpath_exists('/item/value', $1), sql_xml_first_name($1);
EXECUTE sql_xml_prepared('<item name="prepared"><value/></item>'::xml);
DEALLOCATE sql_xml_prepared;
COPY (SELECT id, document, fragments FROM sql_xml_documents ORDER BY id)
  TO STDOUT;

SET xmloption = document;
SHOW xmloption;
SELECT '<root/>'::xml;
SET xmloption = content;
SELECT '<a/><b/>'::xml;

DROP MATERIALIZED VIEW sql_xml_materialized;
DROP VIEW sql_xml_view;
DROP FUNCTION sql_xml_first_name(xml);
DROP TABLE sql_xml_ctas, sql_xml_dml, sql_xml_documents;
