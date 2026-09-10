SELECT normalize('ä', NFC), normalize('ä', NFD),
       normalize('①', NFKC), normalize('각', NFD);
SELECT 'ä' IS NFC NORMALIZED,
       'ä' IS NFD NORMALIZED,
       'ä' IS NOT NFC NORMALIZED,
       '①' IS NFKD NORMALIZED;
SELECT unicode_version(), unicode_assigned(''), unicode_assigned('ab'),
       unicode_assigned('͸');
SELECT unistr('d\0061t\+000061'), unistr('\D83D\DE00'),
       unistr('literal\\slash');

SELECT casefold('Straße İ Σς' COLLATE pg_unicode_fast),
       casefold('Straße İ Σς' COLLATE "C"),
       casefold('Straße İ Σς' COLLATE ucs_basic);
SELECT lower('ÄBCßİΣ' COLLATE "C"),
       upper('äbcßiσ' COLLATE "C"),
       initcap('élan STRAßE' COLLATE "C");
SELECT lower('ÄBCßİΣ' COLLATE pg_unicode_fast),
       upper('äbcßiσ' COLLATE pg_unicode_fast),
       initcap('élan STRAßE' COLLATE pg_unicode_fast);
CREATE COLLATION unicode_fast_copy FROM pg_catalog.pg_unicode_fast;
SELECT upper('straße' COLLATE unicode_fast_copy),
       initcap('ǆungla' COLLATE unicode_fast_copy);

SELECT to_bin(0), to_bin(42), to_bin(-1::int4), to_bin(-1::int8);
SELECT to_oct(0), to_oct(42), to_oct(-1::int4), to_oct(-1::int8);
SELECT to_hex(-1::int4), to_hex(-1::int8);
SELECT to_ascii('ÀÉîõü', 'LATIN1'), to_ascii('ÀÉîõü', 8);

SELECT oid, proname, prorettype, proargtypes::text, pronargs,
       provolatile, proparallel, proisstrict, prosrc,
       pronargdefaults, provariadic
FROM pg_proc
WHERE oid IN (376, 394, 849, 870, 871, 872, 1268, 1317, 1404,
              1845, 1846, 1847, 2087, 2089, 2090, 3058, 3059,
              3539, 3540, 4350, 4351, 4549, 6105, 6160, 6161,
              6198, 6330, 6331, 6332, 6333, 6412)
ORDER BY oid;
SELECT oid, pg_get_function_arguments(oid),
       pg_get_function_identity_arguments(oid)
FROM pg_proc
WHERE oid IN (1268, 3058, 3059, 3539, 4350, 4351)
ORDER BY oid;
SELECT oid, proname, prorettype, proargtypes::text, pronargs,
       provolatile, proparallel, proisstrict, prosrc,
       pronargdefaults, provariadic, proretset, prokind, prolang
FROM pg_proc
WHERE pronamespace = 11
  AND oid <> 1713
  AND proname IN (
      'ascii', 'bit_length', 'btrim', 'casefold', 'char_length',
      'character_length', 'chr', 'concat', 'concat_ws', 'format', 'initcap',
      'is_normalized', 'left', 'length', 'lower', 'lpad', 'ltrim',
      'normalize', 'octet_length', 'overlay', 'parse_ident', 'position',
      'quote_ident', 'quote_literal', 'quote_nullable', 'repeat', 'replace',
      'reverse', 'right', 'rpad', 'rtrim', 'split_part', 'starts_with',
      'string_to_array', 'string_to_table', 'strpos', 'substr', 'substring',
      'to_ascii', 'to_bin', 'to_hex', 'to_oct', 'translate',
      'unicode_assigned', 'unicode_version', 'unistr', 'upper')
ORDER BY oid;
SELECT oid, collname, collprovider, collencoding,
       collcollate, collctype, colllocale, collversion
FROM pg_collation
WHERE oid IN (962, 6411)
ORDER BY oid;

CREATE TABLE text_unicode_complete (
    id integer PRIMARY KEY,
    source text CHECK (source IS NFD NORMALIZED),
    canonical text GENERATED ALWAYS AS (normalize(source, NFC)) STORED,
    folded text GENERATED ALWAYS AS
        (casefold(source COLLATE pg_unicode_fast)) STORED
);
CREATE INDEX text_unicode_complete_canonical
    ON text_unicode_complete (canonical);
CREATE VIEW text_unicode_complete_view AS
SELECT id, canonical, folded, unicode_assigned(source) AS assigned
FROM text_unicode_complete;
INSERT INTO text_unicode_complete (id, source)
VALUES (1, 'ä'), (2, 'Straße');
SELECT * FROM text_unicode_complete_view ORDER BY id;
SELECT id FROM text_unicode_complete WHERE canonical = 'ä';
PREPARE text_unicode_normalize(text) AS
SELECT normalize($1, NFKC), $1 IS NFC NORMALIZED;
EXECUTE text_unicode_normalize('①');
DEALLOCATE text_unicode_normalize;

SELECT normalize('x', XYZ);
SELECT unistr('\xyz');
SELECT unistr('\D83Dliteral');
SELECT unistr('\0000');
SELECT unistr('\U00110000');
SELECT to_ascii('é');
