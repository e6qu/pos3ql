#!/usr/bin/env python3
"""Compare catalog rows and current object lookup against PostgreSQL 18."""

import argparse
import json
from pathlib import Path

import psycopg


# Declared fields verified against the PostgreSQL reference metadata.
CATALOG_SCALAR_COLUMNS = (
    ('pg_aggregate', ('aggkind', 'aggfinalmodify', 'aggmfinalmodify')),
    ('pg_amop', ('amoppurpose',)),
    ('pg_cast', ('castcontext', 'castmethod')),
    ('pg_collation', ('collprovider',)),
    ('pg_conversion', ('connamespace', 'conowner')),
    ('pg_database', ('datfrozenxid', 'datminmxid')),
    ('pg_enum', ('enumtypid', 'enumsortorder', 'enumlabel')),
    ('pg_event_trigger', ('evtenabled',)),
    ('pg_extension', ('extowner', 'extnamespace')),
    ('pg_foreign_data_wrapper', ('fdwowner', 'fdwhandler', 'fdwvalidator')),
    ('pg_foreign_server', ('srvowner', 'srvfdw')),
    ('pg_foreign_table', ('ftrelid', 'ftserver')),
    ('pg_index', ('indnatts', 'indnkeyatts', 'indpred', 'indexprs', 'indoption', 'indcollation')),
    ('pg_indexes', ('schemaname', 'tablename', 'indexname', 'tablespace')),
    ('pg_inherits', ('inhrelid', 'inhparent')),
    ('pg_init_privs', ('objoid', 'classoid', 'privtype')),
    ('pg_language', ('lanowner', 'lanplcallfoid', 'lanvalidator', 'laninline')),
    ('pg_operator', ('oprkind',)),
    ('pg_partitioned_table', ('partrelid', 'partdefid')),
    ('pg_policy', ('polrelid', 'polcmd')),
    ('pg_proc', ('pronargs', 'prokind', 'provolatile', 'proparallel', 'procost', 'prorows', 'pronargdefaults')),
    ('pg_publication', ('pubname', 'pubowner', 'pubgencols')),
    ('pg_publication_namespace', ('pnpubid', 'pnnspid')),
    ('pg_publication_rel', ('prpubid', 'prrelid', 'prqual')),
    ('pg_range', ('rngtypid', 'rngsubtype', 'rngmultitypid')),
    ('pg_rewrite', ('rulename', 'ev_class', 'ev_type', 'ev_enabled')),
    ('pg_seclabels', ('objoid', 'classoid', 'objnamespace')),
    ('pg_sequences', ('schemaname', 'sequencename', 'sequenceowner', 'data_type', 'last_value')),
    ('pg_views', ('schemaname', 'viewname', 'viewowner', 'definition')),
    ('pg_matviews', ('schemaname', 'matviewname', 'matviewowner', 'tablespace', 'hasindexes', 'ispopulated', 'definition')),
    ('pg_rules', ('schemaname', 'tablename', 'rulename', 'definition')),
    ('pg_shdescription', ('objoid', 'classoid')),
    ('pg_shseclabel', ('objoid', 'classoid')),
    ('pg_statistic_ext', ('stxrelid', 'stxnamespace', 'stxowner')),
    ('pg_statistic_ext_data', ('stxoid',)),
    ('pg_subscription_rel', ('srsubid', 'srrelid', 'srsubstate', 'srsublsn')),
    ('pg_trigger', ('tgenabled', 'tgqual')),
    ('pg_type', ('typinput', 'typoutput', 'typdefaultbin')),
    ('pg_user_mapping', ('umuser', 'umserver')),
    ('pg_user_mappings', ('umid', 'srvid', 'umuser')),
)


def connect(host, port, user, database):
    connection = psycopg.connect(
        host=host,
        port=port,
        user=user,
        dbname=database,
        autocommit=True,
        connect_timeout=5,
        prepare_threshold=None,
    )
    connection.execute("SET statement_timeout = '5s'")
    connection.execute("SET lock_timeout = '200ms'")
    return connection


def transcript(reader, writer):
    observed = []

    def record(connection, label, sql, parameters=()):
        cursor = connection.execute(sql, parameters)
        rows = cursor.fetchall()
        observed.append({
            "step": label,
            "types": [column.type_code for column in cursor.description],
            "rows": rows,
        })

    def record_outcome(connection, label, sql, parameters):
        connection.execute("SAVEPOINT catalog_statement_outcome")
        try:
            cursor = connection.execute(sql, parameters)
            rows = cursor.fetchall() if cursor.description is not None else []
            observed.append({
                "step": label,
                "types": [] if cursor.description is None else [
                    column.type_code for column in cursor.description
                ],
                "rows": rows,
            })
        except psycopg.Error as error:
            observed.append({"step": label, "sqlstate": error.sqlstate})
            connection.execute("ROLLBACK TO catalog_statement_outcome")
        finally:
            connection.execute("RELEASE catalog_statement_outcome")

    for isolation in ("READ COMMITTED", "REPEATABLE READ"):
        prefix = isolation.lower().replace(" ", "_")
        for relation in ("views", "sequences", "view_table_usage", "view_column_usage"):
            record(writer, f"{prefix}.information_types.{relation}",
                   f"SELECT * FROM information_schema.{relation} LIMIT 0")
        record(writer, f"{prefix}.derived.builtin_definitions", "SELECT viewname,definition FROM pg_views WHERE schemaname='pg_catalog' AND viewname IN ('pg_views','pg_matviews','pg_sequences') ORDER BY viewname")
        for relation, fields in CATALOG_SCALAR_COLUMNS:
            record(
                writer,
                f"{prefix}.catalog_types.{relation}",
                f"SELECT {','.join(fields)} FROM pg_catalog.{relation} LIMIT 0",
            )

        for function, arguments in (
            ("has_table_privilege", "NULL::oid,NULL::text"),
            ("has_column_privilege", "NULL::oid,NULL::text,NULL::text"),
            ("has_any_column_privilege", "NULL::oid,NULL::text"),
            ("has_sequence_privilege", "NULL::oid,NULL::text"),
            ("has_schema_privilege", "NULL::oid,NULL::text"),
            ("has_type_privilege", "NULL::oid,NULL::text"),
            ("has_language_privilege", "NULL::oid,NULL::text"),
            ("has_function_privilege", "NULL::oid,NULL::text"),
            ("has_database_privilege", "NULL::oid,NULL::text"),
            ("has_tablespace_privilege", "NULL::oid,NULL::text"),
            ("has_foreign_data_wrapper_privilege", "NULL::oid,NULL::text"),
            ("has_server_privilege", "NULL::oid,NULL::text"),
            ("has_largeobject_privilege", "NULL::oid,NULL::text"),
            ("has_parameter_privilege", "NULL::text,NULL::text"),
            ("pg_table_is_visible", "NULL::oid"),
            ("pg_type_is_visible", "NULL::oid"),
            ("pg_function_is_visible", "NULL::oid"),
            ("pg_collation_is_visible", "NULL::oid"),
            ("pg_operator_is_visible", "NULL::oid"),
            ("pg_opclass_is_visible", "NULL::oid"),
            ("pg_opfamily_is_visible", "NULL::oid"),
            ("pg_conversion_is_visible", "NULL::oid"),
            ("pg_statistics_obj_is_visible", "NULL::oid"),
            ("pg_ts_dict_is_visible", "NULL::oid"),
            ("pg_ts_config_is_visible", "NULL::oid"),
            ("pg_relation_is_publishable", "NULL::oid"),
        ):
            for qualifier in ("", "pg_catalog."):
                call = f"{qualifier}{function}({arguments})"
                record(writer, f"{prefix}.catalog_boolean.{qualifier}{function}",
                       f"SELECT {call},pg_typeof({call})::text")

        record(writer, f"{prefix}.vector_array_cast", """
            SELECT ('9 10'::oidvector)::oid[]::text,
                   ('1 -2'::int2vector)::smallint[]::text,
                   array_dims('9 10'::oidvector),
                   array_lower('1 -2'::int2vector,1),
                   array_upper('1 -2'::int2vector,1),
                   ('1 -2'::int2vector)[1],
                   ('9 10'::oidvector)[0],
                   '1 2'::int2vector < '1 10'::int2vector,
                   '9 10'::oidvector = '9 10'::oidvector,
                   '9'::oidvector < '1 2'::oidvector
        """)
        writer.execute("BEGIN")
        try:
            for source, target in (("smallint", "int2vector"), ("oid", "oidvector")):
                for label, value in (("null", "NULL"), ("empty", "'{}'"),
                                     ("bounded", "'[8:9]={9,10}'")):
                    record_outcome(
                        writer, f"{prefix}.rejected_array_to_{target}.{label}",
                        f"SELECT {value}::{source}[]::{target}", (),
                    )
        finally:
            writer.execute("ROLLBACK")

        writer.execute("CREATE VIEW derived_definition_diff AS SELECT 1 AS value")
        try:
            reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
            query = "SELECT position('2' IN definition)>0 FROM pg_views WHERE viewname='derived_definition_diff'"
            record(reader, f"{prefix}.derived.definition.before", query)
            writer.execute("CREATE OR REPLACE VIEW derived_definition_diff AS SELECT 2 AS value")
            record(reader, f"{prefix}.derived.definition.replaced", query)
            record(reader, f"{prefix}.derived.definition.lookup", "SELECT position('2' IN pg_get_viewdef('derived_definition_diff'::regclass))>0")
            record(reader, f"{prefix}.derived.definition.execution", "SELECT * FROM derived_definition_diff")
            reader.execute("CREATE OR REPLACE VIEW derived_definition_diff AS SELECT 3 AS value")
            record(reader, f"{prefix}.derived.definition.own", "SELECT position('3' IN definition)>0 FROM pg_views WHERE viewname='derived_definition_diff'")
            record(reader, f"{prefix}.derived.definition.own_lookup", "SELECT position('3' IN pg_get_viewdef('derived_definition_diff'::regclass))>0")
            reader.execute("ROLLBACK")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("DROP VIEW derived_definition_diff")

        writer.execute("CREATE TABLE derived_rule_target_diff(id integer)")
        writer.execute("CREATE TABLE derived_rule_audit_diff(id integer)")
        writer.execute("CREATE RULE derived_definition_rule_diff AS ON INSERT TO derived_rule_target_diff DO ALSO INSERT INTO derived_rule_audit_diff VALUES(1)")
        try:
            reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
            query = "SELECT position('2' IN definition)>0 FROM pg_rules WHERE rulename='derived_definition_rule_diff'"
            record(reader, f"{prefix}.derived.rule_definition.before", query)
            writer.execute("CREATE OR REPLACE RULE derived_definition_rule_diff AS ON INSERT TO derived_rule_target_diff DO ALSO INSERT INTO derived_rule_audit_diff VALUES(2)")
            record(reader, f"{prefix}.derived.rule_definition.replaced", query)
            record(reader, f"{prefix}.derived.rule_definition.lookup", "SELECT position('2' IN pg_get_ruledef(oid))>0 FROM pg_rewrite WHERE rulename='derived_definition_rule_diff'")
            reader.execute("COMMIT")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("DROP TABLE derived_rule_target_diff CASCADE")
            writer.execute("DROP TABLE derived_rule_audit_diff")

        writer.execute("CREATE TABLE derived_source_diff(id integer, payload text)")
        writer.execute("CREATE VIEW derived_view_diff AS SELECT id,payload FROM derived_source_diff WITH LOCAL CHECK OPTION")
        writer.execute("CREATE MATERIALIZED VIEW derived_materialized_diff AS SELECT id FROM derived_source_diff WITH NO DATA")
        writer.execute("CREATE RULE derived_rule_diff AS ON INSERT TO derived_source_diff DO ALSO NOTHING")
        writer.execute("CREATE SEQUENCE derived_sequence_diff AS integer START 3 INCREMENT 2")
        reports = (
            ("views", "SELECT viewname,viewowner,definition IS NOT NULL FROM pg_views WHERE viewname IN ('derived_view_diff','derived_renamed_view_diff') ORDER BY viewname"),
            ("information_views", "SELECT table_catalog=current_database(),table_schema,table_name,view_definition IS NOT NULL,check_option,is_updatable,is_insertable_into,is_trigger_updatable,is_trigger_deletable,is_trigger_insertable_into FROM information_schema.views WHERE table_name IN ('derived_view_diff','derived_renamed_view_diff') ORDER BY table_name"),
            ("materialized", "SELECT matviewname,tablespace,hasindexes,ispopulated,definition IS NOT NULL FROM pg_matviews WHERE matviewname IN ('derived_materialized_diff','derived_renamed_materialized_diff') ORDER BY matviewname"),
            ("rules", "SELECT tablename,rulename,definition IS NOT NULL FROM pg_rules WHERE rulename IN ('derived_rule_diff','derived_renamed_rule_diff') ORDER BY rulename"),
            ("sequences", "SELECT sequencename,data_type,start_value,min_value,max_value,increment_by,cycle,cache_size FROM pg_sequences WHERE sequencename IN ('derived_sequence_diff','derived_renamed_sequence_diff') ORDER BY sequencename"),
            ("information_sequences", "SELECT sequence_catalog=current_database(),sequence_schema,sequence_name,data_type,numeric_precision,numeric_precision_radix,numeric_scale,start_value,minimum_value,maximum_value,increment,cycle_option FROM information_schema.sequences WHERE sequence_name IN ('derived_sequence_diff','derived_renamed_sequence_diff') ORDER BY sequence_name"),
            ("tables", "SELECT view_catalog=current_database(),view_schema,view_name,table_catalog=current_database(),table_schema,table_name FROM information_schema.view_table_usage WHERE view_name IN ('derived_view_diff','derived_renamed_view_diff') ORDER BY view_name,table_name"),
            ("columns", "SELECT view_name,table_name,column_name FROM information_schema.view_column_usage WHERE view_name IN ('derived_view_diff','derived_renamed_view_diff') ORDER BY view_name,table_name,column_name"),
        )
        changes = (
            "ALTER VIEW derived_view_diff RENAME TO derived_renamed_view_diff",
            "ALTER VIEW derived_renamed_view_diff SET (check_option=cascaded)",
            "ALTER MATERIALIZED VIEW derived_materialized_diff RENAME TO derived_renamed_materialized_diff",
            "CREATE INDEX derived_materialized_index_diff ON derived_renamed_materialized_diff(id)",
            "ALTER RULE derived_rule_diff ON derived_source_diff RENAME TO derived_renamed_rule_diff",
            "ALTER TABLE derived_source_diff RENAME TO derived_renamed_source_diff",
            "CREATE TABLE derived_source_diff(id integer,payload text)",
            "ALTER TABLE derived_renamed_source_diff RENAME payload TO renamed_payload",
            "INSERT INTO derived_renamed_source_diff VALUES(42,'retained')",
            "INSERT INTO derived_source_diff VALUES(99,'replacement')",
            "ALTER SEQUENCE derived_sequence_diff RENAME TO derived_renamed_sequence_diff",
            "ALTER SEQUENCE derived_renamed_sequence_diff INCREMENT 5",
        )
        # Definition, updatability and sequence-value functions can acquire
        # PostgreSQL relation locks. Establish catalog snapshots with listings;
        # evaluate those functions after the concurrent writer has committed.
        listings = (
            ("views", "SELECT viewname,viewowner FROM pg_views WHERE viewname IN ('derived_view_diff','derived_renamed_view_diff')"),
            ("information_views", "SELECT table_name,check_option FROM information_schema.views WHERE table_name IN ('derived_view_diff','derived_renamed_view_diff')"),
            ("materialized", "SELECT matviewname,hasindexes,ispopulated FROM pg_matviews WHERE matviewname IN ('derived_materialized_diff','derived_renamed_materialized_diff')"),
            ("rules", "SELECT tablename,rulename FROM pg_rules WHERE rulename IN ('derived_rule_diff','derived_renamed_rule_diff')"),
            *reports[5:],
        )
        try:
            reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
            for label, sql in listings:
                record(reader, f"{prefix}.derived.before.{label}", sql)
            writer.execute("BEGIN")
            writer.execute("SAVEPOINT derived_change")
            for sql in changes:
                print(f"{prefix}.derived.change: {sql}", flush=True)
                writer.execute(sql)
            for label, sql in reports:
                record(writer, f"{prefix}.derived.private.{label}", sql)
            record(writer, f"{prefix}.derived.private.bound_rows", "SELECT * FROM derived_renamed_view_diff")
            for label, sql in listings:
                record(reader, f"{prefix}.derived.concurrent.{label}", sql)
            writer.execute("ROLLBACK TO derived_change")
            for label, sql in reports:
                record(writer, f"{prefix}.derived.rollback.{label}", sql)
            for sql in changes:
                writer.execute(sql)
            writer.execute("COMMIT")
            writer.execute("SELECT nextval('derived_renamed_sequence_diff')")
            for label, sql in reports:
                record(reader, f"{prefix}.derived.retained.{label}", sql)
            record(reader, f"{prefix}.derived.current_sequence_value", "SELECT last_value FROM pg_sequences WHERE sequencename IN ('derived_sequence_diff','derived_renamed_sequence_diff')")
            reader.execute("COMMIT")
            for label, sql in reports:
                record(reader, f"{prefix}.derived.fresh.{label}", sql)
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP VIEW IF EXISTS derived_view_diff")
            writer.execute("DROP VIEW IF EXISTS derived_renamed_view_diff")
            writer.execute("DROP MATERIALIZED VIEW IF EXISTS derived_materialized_diff")
            writer.execute("DROP MATERIALIZED VIEW IF EXISTS derived_renamed_materialized_diff")
            writer.execute("DROP TABLE IF EXISTS derived_source_diff CASCADE")
            writer.execute("DROP TABLE IF EXISTS derived_renamed_source_diff CASCADE")
            writer.execute("DROP SEQUENCE IF EXISTS derived_sequence_diff")
            writer.execute("DROP SEQUENCE IF EXISTS derived_renamed_sequence_diff")

        writer.execute("CREATE ROLE derived_reader_diff")
        writer.execute("CREATE TABLE derived_private_diff(id integer,payload text)")
        writer.execute("CREATE VIEW derived_repeated_diff AS SELECT a.id,b.id AS other,a.payload FROM derived_private_diff a JOIN derived_private_diff b ON a.id=b.id")
        writer.execute("CREATE SEQUENCE derived_hidden_sequence_diff")
        writer.execute("SELECT nextval('derived_hidden_sequence_diff')")
        writer.execute("GRANT SELECT ON derived_repeated_diff TO derived_reader_diff")
        writer.execute("GRANT SET ON PARAMETER track_functions TO derived_reader_diff")
        writer.execute("CREATE SCHEMA derived_identity_other_diff")
        writer.execute("CREATE VIEW public.derived_identity_view_diff AS SELECT 1 AS value")
        writer.execute("CREATE VIEW derived_identity_other_diff.derived_identity_view_diff AS SELECT 2 AS value")
        writer.execute("CREATE SEQUENCE public.derived_identity_sequence_diff START 4")
        writer.execute("CREATE SEQUENCE derived_identity_other_diff.derived_identity_sequence_diff START 9")
        writer.execute("SELECT nextval('public.derived_identity_sequence_diff'),nextval('derived_identity_other_diff.derived_identity_sequence_diff')")
        writer.execute("GRANT SELECT ON derived_identity_other_diff.derived_identity_view_diff TO derived_reader_diff")
        writer.execute("GRANT USAGE ON SEQUENCE derived_identity_other_diff.derived_identity_sequence_diff TO derived_reader_diff")
        writer.execute("CREATE FUNCTION derived_identity_function_diff() RETURNS integer LANGUAGE sql AS 'SELECT 1'")
        writer.execute("REVOKE ALL ON FUNCTION derived_identity_function_diff() FROM PUBLIC")
        writer.execute("GRANT EXECUTE ON FUNCTION derived_identity_function_diff() TO derived_reader_diff")
        try:
            for role in ("postgres", "derived_reader_diff"):
                reader.execute(f"SET ROLE {role}")
                record(reader, f"{prefix}.derived.privilege.{role}.view", "SELECT table_name,view_definition IS NULL FROM information_schema.views WHERE table_name='derived_repeated_diff'")
                record(reader, f"{prefix}.derived.privilege.{role}.table", "SELECT table_name FROM information_schema.view_table_usage WHERE view_name='derived_repeated_diff'")
                record(reader, f"{prefix}.derived.privilege.{role}.column", "SELECT table_name,column_name FROM information_schema.view_column_usage WHERE view_name='derived_repeated_diff' ORDER BY column_name")
                record(reader, f"{prefix}.derived.privilege.{role}.sequence", "SELECT sequence_name FROM information_schema.sequences WHERE sequence_name='derived_hidden_sequence_diff'")
                record(reader, f"{prefix}.derived.privilege.{role}.last", "SELECT last_value FROM pg_sequences WHERE sequencename='derived_hidden_sequence_diff'")
                record(reader, f"{prefix}.derived.privilege.{role}.any", "SELECT has_table_privilege('derived_repeated_diff','SELECT, INSERT'),has_any_column_privilege('derived_repeated_diff','SELECT, UPDATE'),has_parameter_privilege('track_functions','SET, ALTER SYSTEM')")
                record(reader, f"{prefix}.derived.privilege.{role}.identity", "SELECT n.nspname,has_table_privilege(c.oid,'SELECT, INSERT'),has_any_column_privilege(c.oid,'SELECT, UPDATE'),has_column_privilege(c.oid,1::smallint,'SELECT') FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relname='derived_identity_view_diff' ORDER BY n.nspname")
                for relation, name_field in (("views", "table_name"), ("tables", "table_name"), ("columns", "column_name")):
                    record(reader, f"{prefix}.derived.privilege.{role}.identity.{relation}", f"SELECT table_schema,{name_field} FROM information_schema.{relation} WHERE table_name='derived_identity_view_diff' ORDER BY table_schema")
                record(reader, f"{prefix}.derived.privilege.{role}.identity.sequences", "SELECT sequence_schema,sequence_name FROM information_schema.sequences WHERE sequence_name='derived_identity_sequence_diff' ORDER BY sequence_schema")
                record(reader, f"{prefix}.derived.privilege.{role}.identity.values", "SELECT schemaname,last_value FROM pg_sequences WHERE sequencename='derived_identity_sequence_diff' ORDER BY schemaname")
                record(reader, f"{prefix}.derived.privilege.{role}.identity.function", "SELECT has_function_privilege('derived_identity_function_diff()'::regprocedure::oid,'EXECUTE')")
                record(reader, f"{prefix}.derived.privilege.{role}.builtin", "SELECT relname,has_table_privilege(oid,'SELECT'),has_table_privilege(oid,'INSERT'),has_table_privilege(oid,'SELECT WITH GRANT OPTION') FROM pg_class WHERE relnamespace='pg_catalog'::regnamespace AND relname IN ('pg_authid','pg_largeobject','pg_shadow','pg_statistic','pg_statistic_ext_data','pg_file_settings','pg_hba_file_rules','pg_ident_file_mappings','pg_config','pg_shmem_allocations','pg_shmem_allocations_numa','pg_backend_memory_contexts','pg_user_mapping','pg_replication_origin_status','pg_subscription','pg_aios','pg_views','pg_tables') ORDER BY relname")
                reader.execute("BEGIN")
                try:
                    for label, sql in (
                        ("invalid_table_list", "SELECT has_table_privilege('derived_repeated_diff','SELECT, USAGE')"),
                        ("invalid_table_all", "SELECT has_table_privilege('derived_repeated_diff','ALL')"),
                        ("invalid_parameter_all", "SELECT has_parameter_privilege('track_functions','SET, ALL')"),
                        ("invalid_sequence_list", "SELECT has_sequence_privilege('derived_hidden_sequence_diff','SELECT, INSERT')"),
                        ("invalid_unicode", "SELECT has_table_privilege('derived_repeated_diff','🦀🦀🦀🦀🦀')"),
                    ):
                        record_outcome(reader, f"{prefix}.derived.privilege.{role}.{label}", sql, ())
                finally:
                    reader.execute("ROLLBACK")
        finally:
            reader.execute("RESET ROLE")
            writer.execute("DROP VIEW derived_repeated_diff")
            writer.execute("DROP TABLE derived_private_diff")
            writer.execute("DROP SEQUENCE derived_hidden_sequence_diff")
            writer.execute("DROP SCHEMA derived_identity_other_diff CASCADE")
            writer.execute("DROP VIEW public.derived_identity_view_diff")
            writer.execute("DROP SEQUENCE public.derived_identity_sequence_diff")
            writer.execute("DROP FUNCTION derived_identity_function_diff()")
            writer.execute("REVOKE SET ON PARAMETER track_functions FROM derived_reader_diff")
            writer.execute("DROP ROLE derived_reader_diff")

        writer.execute("CREATE TABLE catalog_index_vectors_diff(id integer, note text, extra integer)")
        try:
            writer.execute('CREATE INDEX catalog_index_vectors_diff_idx ON catalog_index_vectors_diff (id DESC NULLS LAST, note COLLATE "C" ASC NULLS FIRST) INCLUDE (extra)')
            record(writer, f"{prefix}.catalog_index_vectors", """
                SELECT indnatts,indnkeyatts,indoption::text,indcollation::text,
                       pg_typeof(indoption)::text,pg_typeof(indcollation)::text,
                       indoption[0],indoption[1],
                       indcollation[0]=0::oid,indcollation[1]='"C"'::regcollation::oid
                  FROM pg_index WHERE indexrelid='catalog_index_vectors_diff_idx'::regclass
            """)
            record(writer, f"{prefix}.catalog_index_vector_arrays", """
                SELECT array_dims(indoption::smallint[]),array_dims(indcollation::oid[]),
                       (indoption::smallint[])::text,(indoption::smallint[])[0],
                       indoption[-1] IS NULL,indoption[2] IS NULL,
                       '[0:1]={1,2}'::integer[]::bigint[]::text
                  FROM pg_index WHERE indexrelid='catalog_index_vectors_diff_idx'::regclass
            """)
            record(writer, f"{prefix}.catalog_array_type_names", "SELECT 1009::oid::regtype::text,1115::oid::regtype::text,1002::oid::regtype::text,'timestamp[]'::regtype::text")
        finally:
            writer.execute("DROP TABLE catalog_index_vectors_diff")

        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            record(reader, f"{prefix}.own_create.before", "SELECT count(*) FROM pg_class WHERE relname='catalog_own_create_diff'")
            reader.execute("CREATE TABLE catalog_own_create_diff(id integer)")
            record(reader, f"{prefix}.own_create.catalog", "SELECT count(*) FROM pg_class WHERE relname='catalog_own_create_diff'")
            reader.execute("COMMENT ON TABLE catalog_own_create_diff IS 'own created comment'")
            record(reader, f"{prefix}.own_create.comment", "SELECT obj_description('catalog_own_create_diff'::regclass, 'pg_class')")
            reader.execute("INSERT INTO catalog_own_create_diff VALUES(1)")
            record(reader, f"{prefix}.own_create.rows", "SELECT count(*) FROM catalog_own_create_diff")
            reader.execute("ROLLBACK")
            record(reader, f"{prefix}.own_create.rolled_back", "SELECT count(*) FROM pg_class WHERE relname='catalog_own_create_diff'")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("DROP TABLE IF EXISTS catalog_own_create_diff")

        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            record(reader, f"{prefix}.create.before", "SELECT count(*) FROM pg_class WHERE relname = 'catalog_new_diff'")
            writer.execute("BEGIN")
            writer.execute("CREATE TABLE catalog_new_diff(id integer PRIMARY KEY, note text)")
            writer.execute("COMMENT ON TABLE catalog_new_diff IS 'created later'")
            writer.execute("INSERT INTO catalog_new_diff VALUES(1, 'later')")
            writer.execute("COMMIT")
            record(reader, f"{prefix}.create.catalog", "SELECT count(*) FROM pg_class WHERE relname = 'catalog_new_diff'")
            record(reader, f"{prefix}.create.lookup", "SELECT to_regclass('catalog_new_diff') IS NOT NULL")
            record(reader, f"{prefix}.create.columns", "SELECT count(*) FROM pg_attribute WHERE attrelid = to_regclass('catalog_new_diff') AND attnum > 0")
            record(reader, f"{prefix}.create.comment", "SELECT obj_description(to_regclass('catalog_new_diff'), 'pg_class')")
            record(reader, f"{prefix}.create.rows", "SELECT count(*) FROM catalog_new_diff")
            reader.execute("COMMIT")
            record(reader, f"{prefix}.create.fresh", "SELECT count(*) FROM catalog_new_diff")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP TABLE IF EXISTS catalog_new_diff")

        writer.execute("CREATE TABLE catalog_old_diff(id integer PRIMARY KEY, note text)")
        writer.execute("COMMENT ON TABLE catalog_old_diff IS 'old table comment'")
        writer.execute("COMMENT ON COLUMN catalog_old_diff.note IS 'old column comment'")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            oid = reader.execute("SELECT oid FROM pg_class WHERE relname = 'catalog_old_diff'").fetchone()[0]
            record(reader, f"{prefix}.rename.before", "SELECT relname FROM pg_class WHERE oid = %s", (oid,))
            writer.execute("BEGIN")
            writer.execute("ALTER TABLE catalog_old_diff RENAME TO catalog_renamed_diff")
            writer.execute("ALTER TABLE catalog_renamed_diff RENAME COLUMN note TO label")
            writer.execute("COMMENT ON TABLE catalog_renamed_diff IS 'new table comment'")
            writer.execute("COMMENT ON COLUMN catalog_renamed_diff.label IS 'new column comment'")
            writer.execute("COMMIT")
            record(reader, f"{prefix}.rename.catalog", "SELECT relname FROM pg_class WHERE oid = %s", (oid,))
            record(reader, f"{prefix}.rename.columns", "SELECT attname FROM pg_attribute WHERE attrelid = %s AND attnum > 0 ORDER BY attnum", (oid,))
            record(reader, f"{prefix}.rename.lookup", "SELECT to_regclass('catalog_old_diff') IS NULL, to_regclass('catalog_renamed_diff')::oid = %s::oid", (oid,))
            record(reader, f"{prefix}.rename.display", "SELECT %s::oid::regclass::text", (oid,))
            record(reader, f"{prefix}.rename.comments", "SELECT obj_description(%s::oid, 'pg_class'), col_description(%s::oid, 2)", (oid, oid))
            record(reader, f"{prefix}.rename.comment_rows", "SELECT d.description FROM pg_description d WHERE d.classoid = 'pg_class'::regclass AND d.objoid = %s ORDER BY d.objsubid", (oid,))
            record(reader, f"{prefix}.rename.pg_tables", "SELECT tablename,hasindexes FROM pg_tables WHERE schemaname='public' AND tablename IN ('catalog_old_diff','catalog_renamed_diff') ORDER BY tablename")
            record(reader, f"{prefix}.rename.information_tables", "SELECT table_name,table_type FROM information_schema.tables WHERE table_schema='public' AND table_name IN ('catalog_old_diff','catalog_renamed_diff') ORDER BY table_name")
            record(reader, f"{prefix}.rename.information_columns", "SELECT table_name,column_name FROM information_schema.columns WHERE table_schema='public' AND table_name IN ('catalog_old_diff','catalog_renamed_diff') ORDER BY table_name,ordinal_position")
            reader.execute("COMMIT")
            record(reader, f"{prefix}.rename.fresh", "SELECT relname FROM pg_class WHERE oid = %s", (oid,))
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP TABLE IF EXISTS catalog_old_diff")
            writer.execute("DROP TABLE IF EXISTS catalog_renamed_diff")

        writer.execute("CREATE TABLE catalog_comment_private_diff(id integer)")
        writer.execute("COMMENT ON TABLE catalog_comment_private_diff IS 'old comment'")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            oid = reader.execute("SELECT oid FROM pg_class WHERE relname='catalog_comment_private_diff'").fetchone()[0]
            query = "SELECT obj_description(%s::oid, 'pg_class'), (SELECT description FROM pg_description WHERE objoid=%s::oid AND classoid='pg_class'::regclass AND objsubid=0)"
            record(reader, f"{prefix}.comment_private.before", query, (oid, oid))
            writer.execute("COMMENT ON TABLE catalog_comment_private_diff IS 'new comment'")
            record(reader, f"{prefix}.comment_private.concurrent", query, (oid, oid))
            reader.execute("SAVEPOINT before_private_comment")
            reader.execute("COMMENT ON TABLE catalog_comment_private_diff IS 'new comment'")
            record(reader, f"{prefix}.comment_private.equal_write", query, (oid, oid))
            record(reader, f"{prefix}.comment_private.equal_rows", "SELECT description FROM pg_description WHERE objoid=%s::oid AND classoid='pg_class'::regclass AND objsubid=0 ORDER BY description", (oid,))
            record_outcome(reader, f"{prefix}.comment_private.different_write", "COMMENT ON TABLE catalog_comment_private_diff IS 'own comment'", ())
            record(reader, f"{prefix}.comment_private.different_rows", "SELECT description FROM pg_description WHERE objoid=%s::oid AND classoid='pg_class'::regclass AND objsubid=0 ORDER BY description", (oid,))
            record(reader, f"{prefix}.comment_private.different_scan", "SELECT description FROM (SELECT * FROM pg_description OFFSET 0) d WHERE objoid=%s::oid AND classoid='pg_class'::regclass AND objsubid=0 ORDER BY description", (oid,))
            record_outcome(reader, f"{prefix}.comment_private.different_function", "SELECT obj_description(%s::oid, 'pg_class')", (oid,))
            record_outcome(reader, f"{prefix}.comment_private.remove_existing", "COMMENT ON TABLE catalog_comment_private_diff IS NULL", ())
            record(reader, f"{prefix}.comment_private.removed_rows", "SELECT description FROM pg_description WHERE objoid=%s::oid AND classoid='pg_class'::regclass AND objsubid=0 ORDER BY description", (oid,))
            record(reader, f"{prefix}.comment_private.removed_scan", "SELECT description FROM (SELECT * FROM pg_description OFFSET 0) d WHERE objoid=%s::oid AND classoid='pg_class'::regclass AND objsubid=0 ORDER BY description", (oid,))
            record_outcome(reader, f"{prefix}.comment_private.removed_function", "SELECT obj_description(%s::oid, 'pg_class')", (oid,))
            record_outcome(reader, f"{prefix}.comment_private.reinsert", "COMMENT ON TABLE catalog_comment_private_diff IS 'inserted own comment'", ())
            record(reader, f"{prefix}.comment_private.reinserted_rows", "SELECT description FROM pg_description WHERE objoid=%s::oid AND classoid='pg_class'::regclass AND objsubid=0 ORDER BY description", (oid,))
            record(reader, f"{prefix}.comment_private.reinserted_scan", "SELECT description FROM (SELECT * FROM pg_description OFFSET 0) d WHERE objoid=%s::oid AND classoid='pg_class'::regclass AND objsubid=0 ORDER BY description", (oid,))
            record_outcome(reader, f"{prefix}.comment_private.reinserted_function", "SELECT obj_description(%s::oid, 'pg_class')", (oid,))
            reader.execute("ROLLBACK TO before_private_comment")
            record(reader, f"{prefix}.comment_private.rolled_back", query, (oid, oid))
            writer.execute("COMMENT ON TABLE catalog_comment_private_diff IS NULL")
            reader.execute("COMMENT ON TABLE catalog_comment_private_diff IS NULL")
            record(reader, f"{prefix}.comment_private.absent_removal", query, (oid, oid))
            reader.execute("COMMIT")
            record(reader, f"{prefix}.comment_private.fresh", query, (oid, oid))
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP TABLE IF EXISTS catalog_comment_private_diff")

        for concurrent in (0, 1, 2):
            for scope in ("top", "savepoint"):
                label = f"{prefix}.comment_own.{int(concurrent)}.{scope}"
                writer.execute("CREATE TABLE catalog_comment_own_diff(id integer)")
                writer.execute("COMMENT ON TABLE catalog_comment_own_diff IS 'old comment'")
                reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
                try:
                    oid = reader.execute("SELECT oid FROM pg_class WHERE relname='catalog_comment_own_diff'").fetchone()[0]
                    query = "SELECT description FROM pg_description WHERE objoid=%s::oid AND classoid='pg_class'::regclass AND objsubid=0 ORDER BY description"
                    record(reader, f"{label}.before", query, (oid,))
                    if concurrent == 1:
                        writer.execute("COMMENT ON TABLE catalog_comment_own_diff IS 'concurrent comment'")
                    elif concurrent == 2:
                        writer.execute("COMMENT ON TABLE catalog_comment_own_diff IS 'old comment'")
                    if scope == "savepoint":
                        reader.execute("SAVEPOINT own_comment_write")
                    reader.execute("COMMENT ON TABLE catalog_comment_own_diff IS 'own comment'")
                    record(reader, f"{label}.written", query, (oid,))
                    if scope == "savepoint":
                        reader.execute("RELEASE own_comment_write")
                        record(reader, f"{label}.released", query, (oid,))
                    reader.execute("COMMIT")
                    record(reader, f"{label}.committed", query, (oid,))
                finally:
                    reader.execute("ROLLBACK")
                    writer.execute("ROLLBACK")
                    writer.execute("DROP TABLE IF EXISTS catalog_comment_own_diff")

        writer.execute("CREATE TABLE catalog_replace_diff(old_column integer)")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            oid = reader.execute("SELECT oid FROM pg_class WHERE relname = 'catalog_replace_diff'").fetchone()[0]
            writer.execute("BEGIN")
            writer.execute("DROP TABLE catalog_replace_diff")
            writer.execute("CREATE TABLE catalog_replace_diff(new_column text)")
            writer.execute("INSERT INTO catalog_replace_diff VALUES('replacement')")
            writer.execute("COMMIT")
            record(reader, f"{prefix}.replace.catalog", "SELECT oid = %s::oid FROM pg_class WHERE relname = 'catalog_replace_diff'", (oid,))
            record(reader, f"{prefix}.replace.old_columns", "SELECT attname FROM pg_attribute WHERE attrelid = %s AND attnum > 0 ORDER BY attnum", (oid,))
            record(reader, f"{prefix}.replace.lookup", "SELECT to_regclass('catalog_replace_diff')::oid = %s::oid", (oid,))
            record(reader, f"{prefix}.replace.rows", "SELECT count(*) FROM catalog_replace_diff")
            reader.execute("COMMIT")
            record(reader, f"{prefix}.replace.fresh", "SELECT count(*) FROM catalog_replace_diff")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP TABLE IF EXISTS catalog_replace_diff")

        writer.execute("CREATE TYPE catalog_enum_diff AS ENUM('old')")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            record(reader, f"{prefix}.enum.before", "SELECT enumlabel FROM pg_enum WHERE enumtypid = 'catalog_enum_diff'::regtype ORDER BY enumsortorder")
            writer.execute("ALTER TYPE catalog_enum_diff ADD VALUE 'new'")
            record(reader, f"{prefix}.enum.catalog", "SELECT enumlabel FROM pg_enum WHERE enumtypid = 'catalog_enum_diff'::regtype ORDER BY enumsortorder")
            record(reader, f"{prefix}.enum.lookup", "SELECT 'new'::catalog_enum_diff::text")
            reader.execute("COMMIT")
            record(reader, f"{prefix}.enum.fresh", "SELECT enumlabel FROM pg_enum WHERE enumtypid = 'catalog_enum_diff'::regtype ORDER BY enumsortorder")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("DROP TYPE catalog_enum_diff")

        writer.execute("CREATE FUNCTION catalog_routine_diff() RETURNS integer LANGUAGE sql AS 'SELECT 1'")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            record(reader, f"{prefix}.routine.before", "SELECT prosrc FROM pg_proc WHERE proname = 'catalog_routine_diff'")
            writer.execute("CREATE OR REPLACE FUNCTION catalog_routine_diff() RETURNS integer LANGUAGE sql AS 'SELECT 2'")
            record(reader, f"{prefix}.routine.catalog", "SELECT prosrc FROM pg_proc WHERE proname = 'catalog_routine_diff'")
            record(reader, f"{prefix}.routine.call", "SELECT catalog_routine_diff()")
            reader.execute("COMMIT")
            record(reader, f"{prefix}.routine.fresh", "SELECT prosrc FROM pg_proc WHERE proname = 'catalog_routine_diff'")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("DROP FUNCTION catalog_routine_diff()")

        writer.execute("CREATE SEQUENCE catalog_sequence_diff START 1 INCREMENT 1")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            record(reader, f"{prefix}.sequence.before", "SELECT seqincrement FROM pg_sequence WHERE seqrelid = 'catalog_sequence_diff'::regclass")
            writer.execute("ALTER SEQUENCE catalog_sequence_diff INCREMENT 5")
            record(reader, f"{prefix}.sequence.catalog", "SELECT seqincrement FROM pg_sequence WHERE seqrelid = 'catalog_sequence_diff'::regclass")
            record(reader, f"{prefix}.sequence.next", "SELECT nextval('catalog_sequence_diff'), nextval('catalog_sequence_diff')")
            reader.execute("COMMIT")
            record(reader, f"{prefix}.sequence.fresh", "SELECT seqincrement FROM pg_sequence WHERE seqrelid = 'catalog_sequence_diff'::regclass")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("DROP SEQUENCE catalog_sequence_diff")

        writer.execute("CREATE TABLE catalog_statistics_version_diff(id integer)")
        writer.execute("INSERT INTO catalog_statistics_version_diff SELECT generate_series(1,8)")
        writer.execute("ANALYZE catalog_statistics_version_diff")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            oid = reader.execute("SELECT oid FROM pg_class WHERE relname='catalog_statistics_version_diff'").fetchone()[0]
            query = "SELECT relname, reltuples::bigint, relhasindex FROM pg_class WHERE oid=%s"
            record(reader, f"{prefix}.statistics_version.before", query, (oid,))
            writer.execute("CREATE INDEX catalog_statistics_version_index_diff ON catalog_statistics_version_diff(id)")
            record(reader, f"{prefix}.statistics_version.index_created", query, (oid,))
            writer.execute("ALTER TABLE catalog_statistics_version_diff RENAME TO catalog_statistics_version_new_diff")
            record(reader, f"{prefix}.statistics_version.renamed", query, (oid,))
            writer.execute("INSERT INTO catalog_statistics_version_new_diff SELECT generate_series(9,16)")
            writer.execute("ANALYZE catalog_statistics_version_new_diff")
            record(reader, f"{prefix}.statistics_version.analyzed", query, (oid,))
            writer.execute("DROP INDEX catalog_statistics_version_index_diff")
            writer.execute("VACUUM catalog_statistics_version_new_diff")
            record(reader, f"{prefix}.statistics_version.vacuumed", query, (oid,))
            reader.execute("COMMIT")
            record(reader, f"{prefix}.statistics_version.fresh", query, (oid,))
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP TABLE IF EXISTS catalog_statistics_version_diff")
            writer.execute("DROP TABLE IF EXISTS catalog_statistics_version_new_diff")

        writer.execute("CREATE TABLE catalog_statistics_diff(id integer)")
        writer.execute("INSERT INTO catalog_statistics_diff SELECT generate_series(1,8)")
        writer.execute("ANALYZE catalog_statistics_diff")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            index_query = "SELECT c.relhasindex, (SELECT count(*) FROM pg_index i WHERE i.indrelid=c.oid) FROM pg_class c WHERE c.relname='catalog_statistics_diff'"
            estimates_query = "SELECT reltuples::bigint FROM pg_class WHERE relname='catalog_statistics_diff'"
            record(reader, f"{prefix}.statistics.index_before", index_query)
            writer.execute("CREATE INDEX catalog_statistics_index_diff ON catalog_statistics_diff(id)")
            record(reader, f"{prefix}.statistics.index_created", index_query)
            writer.execute("DROP INDEX catalog_statistics_index_diff")
            record(reader, f"{prefix}.statistics.index_dropped", index_query)
            record(reader, f"{prefix}.statistics.estimate_before", estimates_query)
            writer.execute("INSERT INTO catalog_statistics_diff SELECT generate_series(9,16)")
            writer.execute("ANALYZE catalog_statistics_diff")
            record(reader, f"{prefix}.statistics.estimate_analyzed", estimates_query)
            writer.execute("BEGIN")
            writer.execute("INSERT INTO catalog_statistics_diff SELECT generate_series(17,24)")
            writer.execute("ANALYZE catalog_statistics_diff")
            writer.execute("ROLLBACK")
            record(reader, f"{prefix}.statistics.estimate_rolled_back", estimates_query)
            reader.execute("COMMIT")
            record(reader, f"{prefix}.statistics.fresh", estimates_query)
            writer.execute("VACUUM catalog_statistics_diff")
            record(reader, f"{prefix}.statistics.vacuumed", "SELECT relhasindex,reltuples::bigint FROM pg_class WHERE relname='catalog_statistics_diff'")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP TABLE catalog_statistics_diff")

        writer.execute("CREATE TABLE catalog_index_build_diff(id integer)")
        writer.execute("INSERT INTO catalog_index_build_diff SELECT generate_series(1,8)")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            query = "SELECT c.relhasindex, c.reltuples::bigint, (SELECT count(*) FROM pg_index i WHERE i.indrelid=c.oid) FROM pg_class c WHERE c.relname='catalog_index_build_diff'"
            record(reader, f"{prefix}.index_build.before", query)
            writer.execute("BEGIN")
            writer.execute("CREATE INDEX catalog_index_build_index_diff ON catalog_index_build_diff(id)")
            record(writer, f"{prefix}.index_build.private", query)
            record(reader, f"{prefix}.index_build.concurrent", query)
            writer.execute("ROLLBACK")
            record(reader, f"{prefix}.index_build.rolled_back", query)
            writer.execute("VACUUM catalog_index_build_diff")
            record(reader, f"{prefix}.index_build.vacuumed", query)
            reader.execute("COMMIT")
            record(reader, f"{prefix}.index_build.fresh", query)
            writer.execute("BEGIN")
            writer.execute("ANALYZE catalog_index_build_diff")
            writer.execute("SAVEPOINT before_count_change")
            writer.execute("INSERT INTO catalog_index_build_diff SELECT generate_series(9,16)")
            writer.execute("ANALYZE catalog_index_build_diff")
            record(writer, f"{prefix}.index_build.private_analyzed", query)
            writer.execute("ROLLBACK TO before_count_change")
            record(writer, f"{prefix}.index_build.private_counts_after_savepoint", query)
            record(writer, f"{prefix}.index_build.private_rows_after_savepoint", "SELECT count(*) FROM catalog_index_build_diff")
            writer.execute("COMMIT")
            record(reader, f"{prefix}.index_build.committed_counts_after_savepoint", query)
            writer.execute("VACUUM catalog_index_build_diff")
            record(reader, f"{prefix}.index_build.vacuumed_after_savepoint", query)
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP TABLE catalog_index_build_diff")

        writer.execute("CREATE ROLE catalog_permission_reader_diff")
        writer.execute("CREATE TABLE catalog_permission_diff(id integer)")
        writer.execute("GRANT SELECT ON catalog_permission_diff TO catalog_permission_reader_diff")
        large_object = None
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            role_oid = reader.execute("SELECT oid FROM pg_authid WHERE rolname='catalog_permission_reader_diff'").fetchone()[0]
            query = "SELECT relacl::text FROM pg_class WHERE oid='catalog_permission_diff'::regclass"
            current_query = "SELECT pg_get_acl('pg_class'::regclass, 'catalog_permission_diff'::regclass, 0)::text"
            record(reader, f"{prefix}.permissions.before", query)
            writer.execute("GRANT UPDATE ON catalog_permission_diff TO catalog_permission_reader_diff")
            record(reader, f"{prefix}.permissions.catalog", query)
            record(reader, f"{prefix}.permissions.current_acl", current_query)
            writer.execute("ALTER ROLE catalog_permission_reader_diff RENAME TO catalog_permission_renamed_diff")
            record(reader, f"{prefix}.permissions.renamed_acl", query)
            record(reader, f"{prefix}.permissions.renamed_current_acl", current_query)
            record(reader, f"{prefix}.permissions.role_catalog", "SELECT rolname FROM pg_authid WHERE oid=%s::oid", (role_oid,))
            record(reader, f"{prefix}.permissions.role_view", "SELECT rolname FROM pg_roles WHERE oid=%s::oid", (role_oid,))
            record(reader, f"{prefix}.permissions.role_view_columns", "SELECT attname,atttypid::regtype::text FROM pg_attribute WHERE attrelid='pg_roles'::regclass AND attnum>0 ORDER BY attnum")
            record(reader, f"{prefix}.permissions.role_oid_type", "SELECT pg_typeof(oid)::text FROM pg_authid WHERE oid=%s::oid", (role_oid,))
            record(reader, f"{prefix}.permissions.unknown_role", "SELECT pg_get_userbyid(0), pg_get_userbyid(4294967295::oid), pg_get_userbyid(NULL::oid), pg_typeof(pg_get_userbyid(0))::text")
            writer.execute("GRANT SELECT(id) ON catalog_permission_diff TO catalog_permission_renamed_diff")
            # A newly acquired relation lock processes PostgreSQL's pending
            # catalog invalidations (REL_18_6 lmgr.c LockRelationOid). Without
            # this, constant-only queries can reuse an unrefreshed catcache
            # in repeatable read. Synchronize current permission lookup while
            # preserving the transaction snapshot used by the catalog subquery.
            reader.execute("LOCK TABLE catalog_permission_diff IN ACCESS SHARE MODE")
            record(reader, f"{prefix}.permissions.wrapped_columns", "SELECT pg_get_acl('pg_class'::regclass, 'catalog_permission_diff'::regclass, 65537) = pg_get_acl('pg_class'::regclass, 'catalog_permission_diff'::regclass, 1), pg_get_acl('pg_class'::regclass, 'catalog_permission_diff'::regclass, -65535) = pg_get_acl('pg_class'::regclass, 'catalog_permission_diff'::regclass, 1), pg_get_acl('pg_class'::regclass, 'catalog_permission_diff'::regclass, 65536) IS NULL")
            record(reader, f"{prefix}.permissions.wrapped_column_values", "SELECT pg_get_acl('pg_class'::regclass,'catalog_permission_diff'::regclass,65537)::text, pg_get_acl('pg_class'::regclass,'catalog_permission_diff'::regclass,-65535)::text, pg_get_acl('pg_class'::regclass,'catalog_permission_diff'::regclass,1)::text")
            record(reader, f"{prefix}.permissions.wrapped_columns_again", "SELECT pg_get_acl('pg_class'::regclass, 'catalog_permission_diff'::regclass, 65537) = pg_get_acl('pg_class'::regclass, 'catalog_permission_diff'::regclass, 1), pg_get_acl('pg_class'::regclass, 'catalog_permission_diff'::regclass, -65535) = pg_get_acl('pg_class'::regclass, 'catalog_permission_diff'::regclass, 1)")
            record(reader, f"{prefix}.permissions.column_lookup", "SELECT pg_get_acl('pg_class'::regclass,'catalog_permission_diff'::regclass,1)::text, (SELECT attacl::text FROM pg_attribute WHERE attrelid='catalog_permission_diff'::regclass AND attnum=1), has_column_privilege('catalog_permission_renamed_diff','catalog_permission_diff','id','SELECT')")
            writer.execute("CREATE FUNCTION catalog_permission_function_diff() RETURNS integer LANGUAGE sql AS 'SELECT 1'")
            record(writer, f"{prefix}.permissions.function_default_before", "SELECT proacl IS NULL FROM pg_proc WHERE oid='catalog_permission_function_diff()'::regprocedure")
            writer.execute("REVOKE EXECUTE ON FUNCTION catalog_permission_function_diff() FROM catalog_permission_renamed_diff")
            record(writer, f"{prefix}.permissions.function_ungranted_revoke", "SELECT proacl::text,has_function_privilege('catalog_permission_renamed_diff','catalog_permission_function_diff()','EXECUTE') FROM pg_proc WHERE oid='catalog_permission_function_diff()'::regprocedure")
            writer.execute("GRANT EXECUTE ON FUNCTION catalog_permission_function_diff() TO catalog_permission_renamed_diff")
            record(reader, f"{prefix}.permissions.function_subid", "SELECT pg_get_acl('pg_proc'::regclass, 'catalog_permission_function_diff()'::regprocedure, 0)::text, pg_get_acl('pg_proc'::regclass, 'catalog_permission_function_diff()'::regprocedure, 7)::text")
            large_object = writer.execute("SELECT lo_create(0)").fetchone()[0]
            writer.execute(f"GRANT SELECT ON LARGE OBJECT {large_object} TO catalog_permission_renamed_diff")
            record(reader, f"{prefix}.permissions.large_object_alias", "SELECT pg_get_acl('pg_largeobject'::regclass, %s::oid, -7)::text, pg_get_acl('pg_largeobject_metadata'::regclass, %s::oid, 7)::text", (large_object, large_object))
            record(reader, f"{prefix}.permissions.no_acl", "SELECT pg_get_acl('pg_authid'::regclass, %s::oid, 99) IS NULL, pg_get_acl(0,0,99) IS NULL", (role_oid,))
            for class_oid in (0, 1249, 6243, 999999):
                record_outcome(reader, f"{prefix}.permissions.invalid_class_{class_oid}", "SELECT pg_get_acl(%s::oid, %s::oid, 0)", (class_oid, role_oid))
            reader.execute("COMMIT")
            record(reader, f"{prefix}.permissions.fresh", query)
            record(reader, f"{prefix}.permissions.fresh_column_lookup", "SELECT pg_get_acl('pg_class'::regclass,'catalog_permission_diff'::regclass,1)::text, (SELECT attacl::text FROM pg_attribute WHERE attrelid='catalog_permission_diff'::regclass AND attnum=1)")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            if large_object is not None:
                writer.execute("SELECT lo_unlink(%s::oid)", (large_object,))
            writer.execute("DROP FUNCTION IF EXISTS catalog_permission_function_diff()")
            writer.execute("DROP TABLE catalog_permission_diff")
            writer.execute("DROP ROLE IF EXISTS catalog_permission_reader_diff")
            writer.execute("DROP ROLE IF EXISTS catalog_permission_renamed_diff")

        order_roles = ("catalog_order_a_diff", "catalog_order_b_diff", "catalog_order_c_diff")
        for role in order_roles:
            writer.execute(f"CREATE ROLE {role}")
        try:
            writer.execute("CREATE TABLE catalog_acl_released_diff(id integer)")
            writer.execute("CREATE TABLE catalog_acl_order_diff(id integer)")
            writer.execute("REVOKE SELECT ON catalog_acl_order_diff FROM catalog_order_c_diff")
            record(writer, f"{prefix}.permission_order.ungranted_revoke", "SELECT relacl::text FROM pg_class WHERE oid='catalog_acl_order_diff'::regclass")
            writer.execute("GRANT SELECT ON catalog_acl_released_diff TO catalog_order_a_diff")
            writer.execute("GRANT SELECT ON catalog_acl_order_diff TO catalog_order_a_diff")
            writer.execute("DROP TABLE catalog_acl_released_diff")
            writer.execute("GRANT UPDATE ON catalog_acl_order_diff TO catalog_order_b_diff")
            query = "SELECT relacl::text FROM pg_class WHERE oid='catalog_acl_order_diff'::regclass"
            record(writer, f"{prefix}.permission_order.reused_slots", query)
            writer.execute("REVOKE SELECT ON catalog_acl_order_diff FROM catalog_order_a_diff")
            writer.execute("GRANT DELETE ON catalog_acl_order_diff TO catalog_order_c_diff")
            record(writer, f"{prefix}.permission_order.removed_grant", query)
            writer.execute("GRANT SELECT ON catalog_acl_order_diff TO catalog_order_a_diff")
            record(writer, f"{prefix}.permission_order.reinserted_grant", query)
            writer.execute("REVOKE ALL PRIVILEGES ON catalog_acl_order_diff FROM postgres")
            record(writer, f"{prefix}.permission_order.removed_owner", query)
            writer.execute("REVOKE ALL PRIVILEGES ON catalog_acl_order_diff FROM catalog_order_a_diff, catalog_order_b_diff, catalog_order_c_diff")
            record(writer, f"{prefix}.permission_order.empty", query)
            writer.execute("GRANT SELECT ON catalog_acl_order_diff TO catalog_order_a_diff")
            record(writer, f"{prefix}.permission_order.granted_after_empty", query)
            reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
            writer.execute("BEGIN")
            writer.execute("GRANT UPDATE ON catalog_acl_order_diff TO catalog_order_a_diff")
            record_outcome(reader, f"{prefix}.permission_order.conflicting_writer", "GRANT DELETE ON catalog_acl_order_diff TO catalog_order_b_diff", ())
            writer.execute("ROLLBACK")
            record_outcome(reader, f"{prefix}.permission_order.after_owner_release", "GRANT DELETE ON catalog_acl_order_diff TO catalog_order_b_diff", ())
            reader.execute("COMMIT")
            record(writer, f"{prefix}.permission_order.committed_after_wait", query)
            writer.execute("BEGIN")
            writer.execute("GRANT SELECT ON catalog_acl_order_diff TO catalog_order_b_diff")
            reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
            record_outcome(reader, f"{prefix}.permission_order.drop_waits_for_grant", "DROP TABLE catalog_acl_order_diff", ())
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("BEGIN")
            writer.execute("DROP TABLE catalog_acl_order_diff")
            reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
            record_outcome(reader, f"{prefix}.permission_order.grant_waits_for_drop", "GRANT SELECT ON catalog_acl_order_diff TO catalog_order_b_diff", ())
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("ALTER TABLE catalog_acl_order_diff OWNER TO catalog_order_c_diff")
            record(writer, f"{prefix}.permission_order.owner_transfer", query)
            record(writer, f"{prefix}.permission_order.revoked_owner_privilege", "SELECT has_table_privilege('catalog_order_c_diff','catalog_acl_order_diff','SELECT'), has_table_privilege('catalog_order_c_diff','catalog_acl_order_diff','SELECT WITH GRANT OPTION')")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP TABLE IF EXISTS catalog_acl_released_diff")
            writer.execute("DROP TABLE IF EXISTS catalog_acl_order_diff")
            for role in order_roles:
                writer.execute(f"DROP ROLE {role}")

        writer.execute("CREATE DOMAIN catalog_metadata_domain_diff AS varchar(5) NOT NULL")
        writer.execute("CREATE TYPE catalog_metadata_enum_diff AS ENUM ('ready','done')")
        try:
            writer.execute("""CREATE TABLE catalog_metadata_columns_diff(
                id bigint GENERATED ALWAYS AS IDENTITY (START WITH 5 INCREMENT BY 2),
                short_text varchar(7), unlimited_text text, fixed_text char(3),
                fixed_bits bit(6), variable_bits varbit(8),
                amount numeric(12,2), unlimited_numeric numeric,
                day date, clock time, moment timestamptz(3), span interval day to second(2),
                domain_value catalog_metadata_domain_diff,
                enum_value catalog_metadata_enum_diff,
                default_value integer DEFAULT 42,
                generated_value bigint GENERATED ALWAYS AS (id + 1) STORED,
                redundant_value bigint GENERATED ALWAYS AS (((id+1))) STORED,
                product_value bigint GENERATED ALWAYS AS ((id+1)*(id-1)) STORED
            )""")
            record(writer, f"{prefix}.metadata.full_columns", "SELECT * FROM information_schema.columns WHERE table_schema='public' AND table_name='catalog_metadata_columns_diff' ORDER BY ordinal_position")
            record(writer, f"{prefix}.metadata.bit_modifiers", "SELECT attname,atttypmod,format_type(atttypid,atttypmod) FROM pg_attribute WHERE attrelid='catalog_metadata_columns_diff'::regclass AND attname IN ('fixed_bits','variable_bits') ORDER BY attnum")
            record(writer, f"{prefix}.metadata.bit_casts", "SELECT B'101010'::bit(6),B'101'::varbit(8),B'11'::bit,123::bit(8)")
            reader.execute("BEGIN")
            try:
                for type_name in ("bit(0)", "varbit(0)", "bit(83886081)", "varbit(83886081)"):
                    record_outcome(reader, f"{prefix}.metadata.invalid_{type_name}", f"CREATE TABLE catalog_invalid_bits_diff(value {type_name})", ())
            finally:
                reader.execute("ROLLBACK")
            record(writer, f"{prefix}.metadata.builtin_type_io", "SELECT typname,typinput::text,typinput::oid,typoutput::text,typoutput::oid FROM pg_type WHERE typnamespace='pg_catalog'::regnamespace AND typname IN ('int4','text','varchar','numeric','_int4') ORDER BY typname")
            record(writer, f"{prefix}.metadata.user_type_io", "SELECT typname,typinput::text,typinput::oid,typoutput::text,typoutput::oid FROM pg_type WHERE typnamespace='public'::regnamespace AND typname IN ('catalog_metadata_domain_diff','catalog_metadata_enum_diff','_catalog_metadata_domain_diff','_catalog_metadata_enum_diff','catalog_metadata_columns_diff') ORDER BY typname")
            record(writer, f"{prefix}.metadata.full_table", "SELECT * FROM information_schema.tables WHERE table_schema='public' AND table_name='catalog_metadata_columns_diff'")
            record(writer, f"{prefix}.metadata.name_text_join", "SELECT pg_typeof(nspname)::text,nspname FROM pg_namespace JOIN (SELECT 'public'::text AS nspname) names USING(nspname)")
            record(writer, f"{prefix}.metadata.text_name_join", "SELECT pg_typeof(nspname)::text,nspname FROM (SELECT 'public'::text AS nspname) names JOIN pg_namespace USING(nspname)")
            record(writer, f"{prefix}.metadata.hidden_catalog_columns", "SELECT attname FROM pg_attribute WHERE attrelid='pg_namespace'::regclass AND attnum>0 ORDER BY attnum")
            record(writer, f"{prefix}.metadata.catalog_tableoids", "SELECT tableoid='pg_namespace'::regclass FROM pg_namespace WHERE nspname='public'")
        finally:
            writer.execute("DROP TABLE IF EXISTS catalog_metadata_columns_diff")
            writer.execute("DROP DOMAIN catalog_metadata_domain_diff")
            writer.execute("DROP TYPE catalog_metadata_enum_diff")

        writer.execute("CREATE ROLE catalog_parameter_reader_diff")
        writer.execute("CREATE ROLE catalog_parameter_writer_diff")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            count_query = "SELECT count(*) FROM pg_parameter_acl WHERE parname='track_functions'"
            acl_query = "SELECT paracl::text FROM pg_parameter_acl WHERE parname='track_functions'"
            record(reader, f"{prefix}.parameter_acl.before", count_query)
            writer.execute("GRANT SET ON PARAMETER track_functions TO catalog_parameter_reader_diff")
            first_oid = writer.execute("SELECT oid FROM pg_parameter_acl WHERE parname='track_functions'").fetchone()[0]
            record(reader, f"{prefix}.parameter_acl.catalog", count_query)
            record(reader, f"{prefix}.parameter_acl.current_privilege", "SELECT has_parameter_privilege('catalog_parameter_reader_diff', 'track_functions', 'SET')")
            writer.execute("GRANT ALTER SYSTEM ON PARAMETER track_functions TO catalog_parameter_writer_diff")
            record(writer, f"{prefix}.parameter_acl.stable_oid", "SELECT oid=%s::oid FROM pg_parameter_acl WHERE parname='track_functions'", (first_oid,))
            record(writer, f"{prefix}.parameter_acl.complete_acl", acl_query)
            reader.execute("COMMIT")
            record(reader, f"{prefix}.parameter_acl.fresh", acl_query)
            writer.execute("REVOKE SET ON PARAMETER track_functions FROM catalog_parameter_reader_diff")
            writer.execute("REVOKE ALTER SYSTEM ON PARAMETER track_functions FROM catalog_parameter_writer_diff")
            record(reader, f"{prefix}.parameter_acl.default_removed", count_query)
            writer.execute("GRANT SET ON PARAMETER track_functions TO catalog_parameter_reader_diff")
            replacement_oid = writer.execute("SELECT oid FROM pg_parameter_acl WHERE parname='track_functions'").fetchone()[0]
            record(reader, f"{prefix}.parameter_acl.recreated_oid", "SELECT oid<>%s::oid FROM pg_parameter_acl WHERE parname='track_functions'", (first_oid,))
            writer.execute("BEGIN")
            writer.execute("REVOKE SET ON PARAMETER track_functions FROM catalog_parameter_reader_diff")
            writer.execute("GRANT SET ON PARAMETER track_functions TO catalog_parameter_reader_diff")
            record(writer, f"{prefix}.parameter_acl.private_recreated_oid", "SELECT oid<>%s::oid FROM pg_parameter_acl WHERE parname='track_functions'", (replacement_oid,))
            writer.execute("ROLLBACK")
            record(reader, f"{prefix}.parameter_acl.rollback_oid", "SELECT oid=%s::oid FROM pg_parameter_acl WHERE parname='track_functions'", (replacement_oid,))
            writer.execute("REVOKE ALL PRIVILEGES ON PARAMETER track_functions FROM postgres")
            writer.execute("REVOKE SET ON PARAMETER track_functions FROM catalog_parameter_reader_diff")
            record(reader, f"{prefix}.parameter_acl.empty_acl", acl_query)
            writer.execute("GRANT ALL PRIVILEGES ON PARAMETER track_functions TO postgres")
            record(reader, f"{prefix}.parameter_acl.restored_default", count_query)
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP OWNED BY catalog_parameter_reader_diff")
            writer.execute("DROP OWNED BY catalog_parameter_writer_diff")
            writer.execute("DROP ROLE catalog_parameter_reader_diff")
            writer.execute("DROP ROLE catalog_parameter_writer_diff")
    return observed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--database", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--reference", type=Path)
    parser.add_argument("--metadata-output", type=Path)
    arguments = parser.parse_args()
    with connect(arguments.host, arguments.port, arguments.user, arguments.database) as reader, connect(arguments.host, arguments.port, arguments.user, arguments.database) as writer:
        if arguments.metadata_output is not None:
            metadata = {
                "version": reader.execute("SELECT version()").fetchone()[0],
                "catalog_relations": reader.execute("""
                    SELECT c.relname, c.oid, c.relisshared
                      FROM pg_class c
                     WHERE c.relnamespace = 'pg_catalog'::regnamespace
                       AND c.relkind = 'r'
                     ORDER BY c.relname
                """).fetchall(),
                "catalog_keys": reader.execute("""
                    SELECT c.relname, idx.relname,
                           array_agg(a.attname ORDER BY k.ordinality)
                      FROM pg_class c
                      JOIN pg_index i ON i.indrelid = c.oid
                      JOIN pg_class idx ON idx.oid = i.indexrelid
                      CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY k(attnum, ordinality)
                      JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum
                     WHERE c.relnamespace = 'pg_catalog'::regnamespace
                       AND c.relkind = 'r' AND i.indisunique
                       AND k.ordinality <= i.indnkeyatts
                     GROUP BY c.relname, idx.relname, i.indisprimary, idx.oid
                     ORDER BY c.relname, i.indisprimary DESC, idx.oid
                """).fetchall(),
                "catalog_views": reader.execute("""
                    SELECT n.nspname, c.relname, pg_get_viewdef(c.oid, false)
                      FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                     WHERE n.nspname IN ('pg_catalog', 'information_schema')
                       AND c.relkind = 'v'
                     ORDER BY n.nspname, c.relname
                """).fetchall(),
                "catalog_columns": reader.execute("""
                    SELECT n.nspname, c.relname, a.attnum, a.attname,
                           a.atttypid, format_type(a.atttypid, NULL), a.atttypmod
                      FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid
                      JOIN pg_namespace n ON n.oid=c.relnamespace
                     WHERE n.nspname IN ('pg_catalog','information_schema')
                       AND c.relkind IN ('r','v') AND a.attnum>0 AND NOT a.attisdropped
                     ORDER BY n.nspname,c.relname,a.attnum
                """).fetchall(),
                "type_io": reader.execute("""
                    SELECT t.oid, t.typname, t.typtype,
                           t.typinput::oid, t.typinput::text,
                           t.typoutput::oid, t.typoutput::text
                      FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace
                     WHERE n.nspname='pg_catalog'
                     ORDER BY t.oid
                """).fetchall(),
            }
            arguments.metadata_output.write_text(json.dumps(metadata, indent=2) + "\n")
        observed = transcript(reader, writer)
    # JSON normalizes driver tuples before comparing two independent servers.
    serialized = json.dumps(observed, indent=2) + "\n"
    arguments.output.write_text(serialized)
    if arguments.reference is not None:
        expected = json.loads(arguments.reference.read_text())
        actual = json.loads(serialized)
        if expected != actual:
            if len(expected) != len(actual):
                print(f"MISMATCH: observation count: PostgreSQL={len(expected)}, pos3ql={len(actual)}")
            for left, right in zip(expected, actual, strict=False):
                if left != right:
                    expected_outcome = {key: value for key, value in left.items() if key != "step"}
                    actual_outcome = {key: value for key, value in right.items() if key != "step"}
                    print(f"MISMATCH: {left['step']}: PostgreSQL={expected_outcome!r}, pos3ql={actual_outcome!r}")
            raise SystemExit(1)
    print(f"PASS: {len(observed)} catalog snapshot observations on port {arguments.port}")


if __name__ == "__main__":
    main()
