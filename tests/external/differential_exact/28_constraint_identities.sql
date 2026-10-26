CREATE TABLE constraint_parent (id integer PRIMARY KEY);
CREATE TABLE constraint_rules (
 a integer,
 b integer CONSTRAINT rules_nn NOT NULL,
 CONSTRAINT table_first CHECK (a > 0),
 CONSTRAINT table_keep CHECK (b > 0),
 CONSTRAINT table_fk_first FOREIGN KEY (a) REFERENCES constraint_parent (id),
 CONSTRAINT table_fk_keep FOREIGN KEY (b) REFERENCES constraint_parent (id)
);
CREATE DOMAIN constraint_domain AS integer
 CONSTRAINT domain_first CHECK (VALUE > 0)
 CONSTRAINT domain_keep CHECK (VALUE < 10)
 CONSTRAINT domain_nn NOT NULL;
CREATE TABLE saved_constraint_identities AS SELECT conname AS name, oid AS identity
 FROM pg_constraint
 WHERE (conrelid = 'constraint_rules'::regclass AND conname IN ('rules_nn', 'table_keep', 'table_fk_keep'))
    OR (contypid = 'constraint_domain'::regtype AND conname IN ('domain_keep', 'domain_nn'));
CREATE TABLE saved_constraint_triggers AS SELECT tgname AS name, oid AS identity, tgconstraint AS parent
 FROM pg_trigger WHERE tgconstraint = (SELECT identity FROM saved_constraint_identities WHERE name = 'table_fk_keep');
SELECT count(*) = 5 FROM saved_constraint_identities s JOIN pg_constraint c ON c.oid = s.identity;
SELECT count(*) = 4 FROM saved_constraint_triggers s JOIN pg_trigger t ON t.oid = s.identity AND t.tgconstraint = s.parent;
ALTER TABLE constraint_rules DROP CONSTRAINT table_first;
ALTER TABLE constraint_rules DROP CONSTRAINT table_fk_first;
ALTER DOMAIN constraint_domain DROP CONSTRAINT domain_first;
ALTER TABLE constraint_rules RENAME CONSTRAINT table_keep TO constraint_survives;
ALTER TABLE constraint_rules RENAME CONSTRAINT table_fk_keep TO referential_survives;
ALTER DOMAIN constraint_domain RENAME CONSTRAINT domain_keep TO domain_survives;
ALTER TABLE constraint_rules DROP COLUMN a;
ALTER TABLE constraint_rules ALTER COLUMN b SET NOT NULL;
ALTER DOMAIN constraint_domain SET NOT NULL;
SELECT count(*) = 5 FROM saved_constraint_identities s JOIN pg_constraint c ON c.oid = s.identity;
SELECT count(*) = 4 FROM saved_constraint_triggers s JOIN pg_trigger t ON t.oid = s.identity AND t.tgconstraint = s.parent;
BEGIN;
SAVEPOINT before_constraint;
ALTER TABLE constraint_rules DROP CONSTRAINT constraint_survives;
ALTER TABLE constraint_rules ADD CONSTRAINT constraint_survives CHECK (b > 0);
SELECT count(*) = 1 FROM pg_constraint c JOIN saved_constraint_identities s ON s.name = 'table_keep'
 WHERE c.conrelid = 'constraint_rules'::regclass AND c.conname = 'constraint_survives' AND c.oid <> s.identity;
ROLLBACK TO SAVEPOINT before_constraint;
SELECT count(*) = 1 FROM pg_constraint c JOIN saved_constraint_identities s ON c.oid = s.identity
 WHERE s.name = 'table_keep' AND c.conname = 'constraint_survives';
COMMIT;
ALTER TABLE constraint_rules DROP CONSTRAINT constraint_survives;
ALTER TABLE constraint_rules ADD CONSTRAINT constraint_survives CHECK (b > 0);
ALTER TABLE constraint_rules DROP CONSTRAINT referential_survives;
ALTER TABLE constraint_rules ADD CONSTRAINT referential_survives FOREIGN KEY (b) REFERENCES constraint_parent (id);
ALTER TABLE constraint_rules ALTER COLUMN b DROP NOT NULL;
ALTER TABLE constraint_rules ALTER COLUMN b SET NOT NULL;
ALTER DOMAIN constraint_domain DROP CONSTRAINT domain_survives;
ALTER DOMAIN constraint_domain ADD CONSTRAINT domain_survives CHECK (VALUE < 10);
ALTER DOMAIN constraint_domain DROP NOT NULL;
ALTER DOMAIN constraint_domain SET NOT NULL;
SELECT count(*) = 0 FROM saved_constraint_identities s JOIN pg_constraint c ON c.oid = s.identity;
SELECT count(*) = 0 FROM saved_constraint_triggers s JOIN pg_trigger t ON t.oid = s.identity;
SELECT count(DISTINCT oid) = 5 FROM pg_constraint
 WHERE conrelid = 'constraint_rules'::regclass OR contypid = 'constraint_domain'::regtype;
CREATE TABLE current_constraint_identities AS SELECT oid AS identity FROM pg_constraint
 WHERE conrelid = 'constraint_rules'::regclass OR contypid = 'constraint_domain'::regtype;
DROP TABLE constraint_rules;
DROP DOMAIN constraint_domain;
CREATE TABLE constraint_rules (b integer NOT NULL, CONSTRAINT constraint_survives CHECK (b > 0),
 CONSTRAINT referential_survives FOREIGN KEY (b) REFERENCES constraint_parent (id));
CREATE DOMAIN constraint_domain AS integer CHECK (VALUE < 10) NOT NULL;
SELECT count(*) = 0 FROM current_constraint_identities s JOIN pg_constraint c ON c.oid = s.identity;
SELECT count(*) = 0 FROM saved_constraint_identities s JOIN pg_constraint c ON c.oid = s.identity;
SELECT count(DISTINCT oid) = 5 FROM pg_constraint
 WHERE conrelid = 'constraint_rules'::regclass OR contypid = 'constraint_domain'::regtype;
DROP TABLE constraint_rules, constraint_parent, saved_constraint_identities, saved_constraint_triggers, current_constraint_identities;
DROP DOMAIN constraint_domain;
