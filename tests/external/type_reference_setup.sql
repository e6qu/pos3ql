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

CREATE OPERATOR type_scope.## (
    FUNCTION = type_scope.echo_domain, RIGHTARG = type_scope.shared_domain);
CREATE FUNCTION type_scope.equal_domain(a type_scope.shared_domain, b type_scope.shared_domain)
RETURNS boolean LANGUAGE SQL IMMUTABLE AS 'SELECT a::integer = b::integer';
CREATE FUNCTION type_scope.compare_domain(a type_scope.shared_domain, b type_scope.shared_domain)
RETURNS integer LANGUAGE SQL IMMUTABLE
AS 'SELECT CASE WHEN a::integer < b::integer THEN -1 WHEN a::integer > b::integer THEN 1 ELSE 0 END';
CREATE OPERATOR type_scope.=== (
    FUNCTION = type_scope.equal_domain,
    LEFTARG = type_scope.shared_domain, RIGHTARG = type_scope.shared_domain);
CREATE OPERATOR FAMILY type_scope.domain_ops USING btree;
CREATE OPERATOR CLASS type_scope.domain_ops FOR TYPE type_scope.shared_domain
USING btree FAMILY type_scope.domain_ops AS
    OPERATOR 3 type_scope.===,
    FUNCTION 1 type_scope.compare_domain(type_scope.shared_domain, type_scope.shared_domain);
CREATE CAST (type_scope.shared_enum AS varchar) WITH INOUT;
