CREATE TABLE generated_expression_evolution_rows (
  source integer,
  computed integer GENERATED ALWAYS AS (source + 1) STORED
);
INSERT INTO generated_expression_evolution_rows (source) VALUES (2), (4);
ALTER TABLE generated_expression_evolution_rows
  ALTER COLUMN computed SET EXPRESSION AS (source * 10);
SELECT source, computed FROM generated_expression_evolution_rows ORDER BY source;
UPDATE generated_expression_evolution_rows SET source = 3 WHERE source = 2;
SELECT source, computed FROM generated_expression_evolution_rows ORDER BY source;
DROP TABLE generated_expression_evolution_rows;
