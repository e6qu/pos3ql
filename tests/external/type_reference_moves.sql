ALTER DOMAIN type_scope.shared_domain RENAME TO renamed_domain;
ALTER DOMAIN type_scope.renamed_domain SET SCHEMA type_after;
ALTER TYPE type_scope.shared_enum RENAME TO renamed_enum;
ALTER TYPE type_scope.renamed_enum SET SCHEMA type_after;
ALTER TYPE type_scope.shared_record RENAME TO renamed_record;
ALTER TYPE type_scope.renamed_record SET SCHEMA type_after;
