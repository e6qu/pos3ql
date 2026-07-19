-- bit-string literals and hex literals
SELECT B'1010', X'1F', B'', X'a';
-- concatenation (varbit)
SELECT B'101' || B'010', B'1' || B'00';
-- bitwise ops (equal length)
SELECT B'1010' & B'0110', B'1010' | B'0110', B'1010' # B'0110';
-- NOT
SELECT ~ B'1010';
-- shifts (length preserved, zero-filled)
SELECT B'1010' << 1, B'1010' >> 1, B'1010' << 2, B'0001' >> 2;
-- comparisons
SELECT B'10' < B'11', B'10' = B'10', B'100' > B'10', B'1' < B'10';
-- length functions
SELECT length(B'10101'), bit_length(B'10101'), octet_length(B'10101'), octet_length(B'1010101010');
-- casts: bit <-> int
SELECT B'1010'::int, B'11111111'::int, 42::bit(8), 5::bit(4), 255::bit(8);
SELECT (-1)::bit(8), 1::bit(1);
-- casts: text <-> bit
SELECT '1010'::bit(4), '1010'::varbit, B'101'::text;
-- length coercion: bit(n) pad/truncate on the right, varbit(n) truncate
SELECT '101'::bit(5), '10110'::bit(3), '10110'::varbit(3);
-- into a table
CREATE TABLE bt (a bit(4), b varbit);
INSERT INTO bt VALUES (B'1100', B'101010'), (B'0011', B'1');
SELECT a, b, a | B'0001', length(b) FROM bt ORDER BY a;
DROP TABLE bt;
-- negative shift (opposite direction), zero shift
SELECT B'1010' << -1, B'1010' >> -1, B'1010' << 0;
-- bit varying with explicit length keyword
SELECT '10101'::bit varying(3), '101'::bit varying(8);
-- comparison across lengths and equality of fixed vs varbit
SELECT B'101' = B'101'::varbit, B'1010' <> B'1011';
-- NULL propagation
SELECT B'101' | NULL::bit(3), length(NULL::varbit), NULL::bit(4);
-- persisted round-trip after implied restart path (checkpoint not needed here)
CREATE TABLE bt2 (id int, v bit(8));
INSERT INTO bt2 VALUES (1, B'11110000'), (2, 170::bit(8));
SELECT id, v, v & B'00001111', ~v FROM bt2 ORDER BY id;
DROP TABLE bt2;
