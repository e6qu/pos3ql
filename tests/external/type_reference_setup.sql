CREATE SCHEMA type_scope;
CREATE SCHEMA type_after;
CREATE DOMAIN type_scope.shared_domain AS integer CHECK (VALUE > 0);
CREATE DOMAIN type_scope.child_domain AS type_scope.shared_domain;
CREATE TYPE type_scope.shared_enum AS ENUM ('ready');
CREATE TYPE type_scope.drop_enum AS ENUM ('ready');
CREATE TYPE type_scope.shared_record AS (id type_scope.shared_domain);
CREATE TABLE type_scope.items(id type_scope.shared_domain,
                              states type_scope.shared_enum[],
                              payload type_scope.shared_record);
CREATE FUNCTION type_scope.echo_domain(arg type_scope.shared_domain)
RETURNS type_scope.shared_domain LANGUAGE SQL AS 'SELECT arg';
COMMENT ON DOMAIN type_scope.shared_domain IS 'domain comment';
COMMENT ON TYPE type_scope.shared_enum IS 'enum comment';
COMMENT ON TYPE type_scope.shared_record IS 'record comment';
INSERT INTO type_scope.items VALUES (7, ARRAY['ready']::type_scope.shared_enum[], ROW(7));
