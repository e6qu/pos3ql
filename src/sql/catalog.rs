//! `pg_catalog` and `information_schema` as synthesized read-only tables.
//!
//! Drivers and ORMs introspect these to discover relations and columns.
//! Rather than store them, we materialize the rows on demand from the live
//! catalog into the statement arena and hand them to the normal query
//! pipeline as a synthetic table, so WHERE / projection / ORDER BY / LIMIT
//! and joins all work against them.

use crate::mem::arena::Arena;
use crate::storage::{ColumnMeta, MAX_COLUMNS, SqlName, Storage, TableDef};
use crate::util::StackStr;
use crate::{sql_err, stack_format};

use super::eval::{SqlError, sqlstate};
use super::types::{ColType, Datum, TypeMod};

/// A materialized catalog relation: its shape plus rows in the arena.
pub struct SynthTable<'a> {
    pub def: &'a TableDef,
    pub rows: &'a [&'a [Datum<'a>]],
}

/// Stable per-name OIDs so a table's oid is consistent within a session.
/// User relations start above the reserved range.
const FIRST_USER_OID: i32 = 16384;
const PUBLIC_NS_OID: i32 = 2200;
const PG_CATALOG_NS_OID: i32 = 11;
/// Well-known catalog OIDs, for `pg_description.classoid`.
const PG_CLASS_OID: i32 = 1259;
const PG_NAMESPACE_OID: i32 = 2615;
const PG_TYPE_OID: i32 = 1247;

#[derive(Clone, Copy)]
struct IntrinsicRoutine {
    oid: i32,
    name: &'static str,
    result_oid: i32,
    argument_types: &'static str,
    argument_count: i32,
    volatility: &'static str,
}

const INTRINSIC_ROUTINES: &[IntrinsicRoutine] = &[
    IntrinsicRoutine {
        oid: 89,
        name: "version",
        result_oid: 25,
        argument_types: "",
        argument_count: 0,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 861,
        name: "current_database",
        result_oid: 19,
        argument_types: "",
        argument_count: 0,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1081,
        name: "format_type",
        result_oid: 25,
        argument_types: "26 23",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1215,
        name: "obj_description",
        result_oid: 25,
        argument_types: "26 19",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1216,
        name: "col_description",
        result_oid: 25,
        argument_types: "26 23",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1264,
        name: "pg_char_to_encoding",
        result_oid: 23,
        argument_types: "19",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1387,
        name: "pg_get_constraintdef",
        result_oid: 25,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1402,
        name: "current_schema",
        result_oid: 19,
        argument_types: "",
        argument_count: 0,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1403,
        name: "current_schemas",
        result_oid: 1003,
        argument_types: "16",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1597,
        name: "pg_encoding_to_char",
        result_oid: 19,
        argument_types: "23",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1641,
        name: "pg_get_viewdef",
        result_oid: 25,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1642,
        name: "pg_get_userbyid",
        result_oid: 19,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1643,
        name: "pg_get_indexdef",
        result_oid: 25,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1716,
        name: "pg_get_expr",
        result_oid: 25,
        argument_types: "194 26",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 2077,
        name: "current_setting",
        result_oid: 25,
        argument_types: "25",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 2078,
        name: "set_config",
        result_oid: 25,
        argument_types: "25 25 16",
        argument_count: 3,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 2079,
        name: "pg_table_is_visible",
        result_oid: 16,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 2080,
        name: "pg_type_is_visible",
        result_oid: 16,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 2081,
        name: "pg_function_is_visible",
        result_oid: 16,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 2168,
        name: "pg_database_size",
        result_oid: 20,
        argument_types: "19",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 2322,
        name: "pg_tablespace_size",
        result_oid: 20,
        argument_types: "26",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 2997,
        name: "pg_table_size",
        result_oid: 20,
        argument_types: "2205",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3778,
        name: "pg_tablespace_location",
        result_oid: 25,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 3810,
        name: "pg_is_in_recovery",
        result_oid: 16,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3815,
        name: "pg_collation_is_visible",
        result_oid: 16,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 6121,
        name: "pg_relation_is_publishable",
        result_oid: 16,
        argument_types: "2205",
        argument_count: 1,
        volatility: "s",
    },
];

fn catalog_relation_oid(name: &str) -> Option<i32> {
    Some(match name {
        "pg_type" => PG_TYPE_OID,
        "pg_proc" => 1255,
        "pg_class" => PG_CLASS_OID,
        "pg_attribute" => 1249,
        "pg_amop" => 2602,
        "pg_amproc" => 2603,
        "pg_cast" => 2605,
        "pg_constraint" => 2606,
        "pg_depend" => 2608,
        "pg_rewrite" => 2618,
        "pg_namespace" => PG_NAMESPACE_OID,
        "pg_opfamily" => 2753,
        "pg_extension" => 3079,
        "pg_default_acl" => 826,
        "pg_replication_slots" => 121,
        "pg_transform" => 3576,
        _ => return None,
    })
}

pub fn is_catalog_relation(qualifier: Option<&str>, name: &str) -> bool {
    match qualifier {
        Some("pg_catalog") => true,
        Some("information_schema") => matches!(
            name,
            "tables"
                | "columns"
                | "schemata"
                | "table_constraints"
                | "key_column_usage"
                | "constraint_column_usage"
                | "referential_constraints"
                | "table_privileges"
                | "role_table_grants"
                | "sequences"
                | "usage_privileges"
                | "domains"
                | "domain_constraints"
                | "check_constraints"
                | "column_domain_usage"
                | "column_udt_usage"
                | "domain_udt_usage"
                | "collations"
                | "collation_character_set_applicability"
                | "applicable_roles"
                | "administrable_role_authorizations"
                | "enabled_roles"
                | "column_privileges"
                | "role_column_grants"
                | "views"
                | "view_table_usage"
                | "view_column_usage"
                | "routines"
                | "parameters"
                | "routine_privileges"
                | "role_routine_grants"
        ),
        Some(_) => false,
        None => matches!(
            name,
            "pg_class"
                | "pg_attribute"
                | "pg_type"
                | "pg_namespace"
                | "pg_tables"
                | "pg_indexes"
                | "pg_views"
                | "pg_matviews"
                | "pg_sequences"
                | "pg_sequence"
                | "pg_roles"
                | "pg_authid"
                | "pg_database"
                | "pg_am"
                | "pg_index"
                | "pg_constraint"
                | "pg_attrdef"
                | "pg_collation"
                | "pg_conversion"
                | "pg_policy"
                | "pg_rewrite"
                | "pg_trigger"
                | "pg_event_trigger"
                | "pg_inherits"
                | "pg_stats"
                | "pg_statistic_ext"
                | "pg_publication"
                | "pg_publication_rel"
                | "pg_publication_namespace"
                | "pg_replication_slots"
                | "pg_subscription"
                | "pg_subscription_rel"
                | "pg_foreign_table"
                | "pg_foreign_server"
                | "pg_partitioned_table"
                | "pg_description"
                | "pg_seclabels"
                | "pg_shseclabel"
                | "pg_largeobject_metadata"
                | "pg_largeobject"
                | "pg_enum"
                | "pg_range"
                | "pg_settings"
                | "pg_proc"
                | "pg_operator"
                | "pg_opclass"
                | "pg_opfamily"
                | "pg_amop"
                | "pg_amproc"
                | "pg_ts_parser"
                | "pg_ts_template"
                | "pg_ts_dict"
                | "pg_ts_config"
                | "pg_ts_config_map"
                | "pg_language"
                | "pg_auth_members"
                | "pg_db_role_setting"
                | "pg_parameter_acl"
                | "pg_default_acl"
                | "pg_extension"
                | "pg_depend"
                | "pg_init_privs"
                | "pg_cast"
                | "pg_transform"
                | "pg_tablespace"
                | "pg_foreign_data_wrapper"
        ),
    }
}

/// Builds the requested catalog relation. `qualifier` is the schema (or
/// None). Allocates rows in `arena`.
pub fn synthesize<'a>(
    storage: &Storage,
    qualifier: Option<&str>,
    name: &'a str,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let info = qualifier == Some("information_schema");
    match (info, name) {
        (false, "pg_class") => pg_class(storage, txid, arena),
        (false, "pg_attribute") => pg_attribute(storage, txid, arena),
        (false, "pg_attrdef") => pg_attrdef(storage, txid, arena),
        (false, "pg_collation") => pg_collation(arena),
        (false, "pg_conversion") => finish(
            def_of(
                "pg_conversion",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("conname", ColType::Name),
                    ("connamespace", ColType::Int4),
                    ("conowner", ColType::Int4),
                    ("conforencoding", ColType::Int4),
                    ("contoencoding", ColType::Int4),
                    ("conproc", ColType::Int4),
                    ("condefault", ColType::Bool),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_type") => pg_type(storage, txid, arena),
        (false, "pg_namespace") => pg_namespace(storage, txid, arena),
        (false, "pg_tables") => pg_tables(storage, txid, arena),
        (false, "pg_indexes") => pg_indexes(storage, txid, arena),
        (false, "pg_am") => finish(
            def_of(
                "pg_am",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("amname", ColType::Text),
                    ("amhandler", ColType::Int4),
                    ("amtype", ColType::Bpchar),
                ],
            ),
            &[
                row(
                    &[
                        Datum::Int4(2601),
                        Datum::Int4(403),
                        text("btree", arena)?,
                        Datum::Int4(0),
                        text("i", arena)?,
                    ],
                    arena,
                )?,
                row(
                    &[
                        Datum::Int4(2601),
                        Datum::Int4(405),
                        text("hash", arena)?,
                        Datum::Int4(0),
                        text("i", arena)?,
                    ],
                    arena,
                )?,
            ],
            arena,
        ),
        (false, "pg_constraint") => pg_constraint(storage, txid, arena),
        (false, "pg_index") => pg_index(storage, txid, arena),
        (false, "pg_stats") => pg_stats(storage, txid, arena),
        (false, "pg_policy") => finish(
            def_of(
                "pg_policy",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("polname", ColType::Text),
                    ("polrelid", ColType::Int4),
                    ("polcmd", ColType::Bpchar),
                    ("polpermissive", ColType::Bool),
                    ("polroles", ColType::Array(super::types::ArrElem::Int4)),
                    ("polqual", ColType::Text),
                    ("polwithcheck", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_statistic_ext") => finish(
            def_of(
                "pg_statistic_ext",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("stxrelid", ColType::Int4),
                    ("stxnamespace", ColType::Int4),
                    ("stxname", ColType::Text),
                    ("stxowner", ColType::Int4),
                    ("stxkind", ColType::Array(super::types::ArrElem::Text)),
                    ("stxstattarget", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_publication") => pg_publication(storage, txid, arena),
        (false, "pg_publication_namespace") => pg_publication_namespace(storage, txid, arena),
        (false, "pg_publication_rel") => pg_publication_rel(storage, txid, arena),
        (false, "pg_replication_slots") => pg_replication_slots(storage, arena),
        (false, "pg_subscription") => finish(
            def_of(
                "pg_subscription",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("subdbid", ColType::Int4),
                    ("subskiplsn", ColType::Text),
                    ("subname", ColType::Name),
                    ("subowner", ColType::Int4),
                    ("subenabled", ColType::Bool),
                    ("subbinary", ColType::Bool),
                    ("substream", ColType::Bpchar),
                    ("subtwophasestate", ColType::Bpchar),
                    ("subdisableonerr", ColType::Bool),
                    ("subpasswordrequired", ColType::Bool),
                    ("subrunasowner", ColType::Bool),
                    ("subfailover", ColType::Bool),
                    ("subconninfo", ColType::Text),
                    ("subslotname", ColType::Name),
                    ("subsynccommit", ColType::Text),
                    (
                        "subpublications",
                        ColType::Array(super::types::ArrElem::Text),
                    ),
                    ("suborigin", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_subscription_rel") => finish(
            def_of(
                "pg_subscription_rel",
                &[
                    ("srsubid", ColType::Int4),
                    ("srrelid", ColType::Int4),
                    ("srsubstate", ColType::Bpchar),
                    ("srsublsn", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_inherits") => finish(
            def_of(
                "pg_inherits",
                &[
                    ("inhrelid", ColType::Int4),
                    ("inhparent", ColType::Int4),
                    ("inhseqno", ColType::Int4),
                    ("inhdetachpending", ColType::Bool),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_rewrite") => pg_rewrite(storage, txid, arena),
        (false, "pg_trigger") => finish(
            def_of(
                "pg_trigger",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("tgname", ColType::Text),
                    ("tgrelid", ColType::Int4),
                    ("tgenabled", ColType::Bpchar),
                    ("tgisinternal", ColType::Bool),
                    ("tgconstraint", ColType::Int4),
                    ("tgfoid", ColType::Int4),
                    ("tgparentid", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_event_trigger") => finish(
            def_of(
                "pg_event_trigger",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("evtname", ColType::Name),
                    ("evtevent", ColType::Name),
                    ("evtowner", ColType::Int4),
                    ("evtfoid", ColType::Int4),
                    ("evtenabled", ColType::Bpchar),
                    ("evttags", ColType::Array(super::types::ArrElem::Text)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_foreign_table") => finish(
            def_of(
                "pg_foreign_table",
                &[
                    ("ftrelid", ColType::Int4),
                    ("ftserver", ColType::Int4),
                    ("ftoptions", ColType::Array(super::types::ArrElem::Text)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_foreign_server") => finish(
            def_of(
                "pg_foreign_server",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("srvname", ColType::Text),
                    ("srvowner", ColType::Int4),
                    ("srvfdw", ColType::Int4),
                    ("srvtype", ColType::Text),
                    ("srvversion", ColType::Text),
                    ("srvacl", ColType::Array(super::types::ArrElem::Text)),
                    ("srvoptions", ColType::Array(super::types::ArrElem::Text)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_foreign_data_wrapper") => finish(
            def_of(
                "pg_foreign_data_wrapper",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("fdwname", ColType::Text),
                    ("fdwowner", ColType::Int4),
                    ("fdwhandler", ColType::Int4),
                    ("fdwvalidator", ColType::Int4),
                    ("fdwacl", ColType::Array(super::types::ArrElem::Text)),
                    ("fdwoptions", ColType::Array(super::types::ArrElem::Text)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_partitioned_table") => finish(
            def_of(
                "pg_partitioned_table",
                &[
                    ("partrelid", ColType::Int4),
                    ("partstrat", ColType::Bpchar),
                    ("partattrs", ColType::Text),
                    ("partexprs", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_settings") => finish(
            def_of(
                "pg_settings",
                &[
                    ("name", ColType::Text),
                    ("setting", ColType::Text),
                    ("source", ColType::Text),
                    ("boot_val", ColType::Text),
                    ("vartype", ColType::Text),
                    ("context", ColType::Text),
                ],
            ),
            &[
                row(
                    &[
                        text("max_index_keys", arena)?,
                        text("32", arena)?,
                        text("default", arena)?,
                        text("32", arena)?,
                        text("integer", arena)?,
                        text("internal", arena)?,
                    ],
                    arena,
                )?,
                row(
                    &[
                        text("max_identifier_length", arena)?,
                        text("63", arena)?,
                        text("default", arena)?,
                        text("63", arena)?,
                        text("integer", arena)?,
                        text("internal", arena)?,
                    ],
                    arena,
                )?,
                row(
                    &[
                        text("server_version", arena)?,
                        text("18.4", arena)?,
                        text("default", arena)?,
                        text("18.4", arena)?,
                        text("string", arena)?,
                        text("internal", arena)?,
                    ],
                    arena,
                )?,
                row(
                    &[
                        text("server_encoding", arena)?,
                        text("UTF8", arena)?,
                        text("default", arena)?,
                        text("UTF8", arena)?,
                        text("string", arena)?,
                        text("internal", arena)?,
                    ],
                    arena,
                )?,
                row(
                    &[
                        text("standard_conforming_strings", arena)?,
                        text("on", arena)?,
                        text("default", arena)?,
                        text("on", arena)?,
                        text("bool", arena)?,
                        text("user", arena)?,
                    ],
                    arena,
                )?,
            ],
            arena,
        ),
        (false, "pg_proc") => pg_proc(storage, txid, arena),
        (false, "pg_operator") => finish(
            def_of(
                "pg_operator",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("oprname", ColType::Name),
                    ("oprnamespace", ColType::Int4),
                    ("oprowner", ColType::Int4),
                    ("oprkind", ColType::Bpchar),
                    ("oprleft", ColType::Int4),
                    ("oprright", ColType::Int4),
                    ("oprcode", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_opclass") => finish(
            def_of(
                "pg_opclass",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("opcmethod", ColType::Int4),
                    ("opcname", ColType::Name),
                    ("opcnamespace", ColType::Int4),
                    ("opcowner", ColType::Int4),
                    ("opcfamily", ColType::Int4),
                    ("opcintype", ColType::Int4),
                    ("opcdefault", ColType::Bool),
                    ("opckeytype", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_opfamily") => finish(
            def_of(
                "pg_opfamily",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("opfmethod", ColType::Int4),
                    ("opfname", ColType::Name),
                    ("opfnamespace", ColType::Int4),
                    ("opfowner", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_amop") => finish(
            def_of(
                "pg_amop",
                &[
                    ("oid", ColType::Int4),
                    ("amopfamily", ColType::Int4),
                    ("amoplefttype", ColType::Int4),
                    ("amoprighttype", ColType::Int4),
                    ("amopstrategy", ColType::Int4),
                    ("amoppurpose", ColType::Bpchar),
                    ("amopopr", ColType::Int4),
                    ("amopmethod", ColType::Int4),
                    ("amopsortfamily", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_amproc") => finish(
            def_of(
                "pg_amproc",
                &[
                    ("oid", ColType::Int4),
                    ("amprocfamily", ColType::Int4),
                    ("amproclefttype", ColType::Int4),
                    ("amprocrighttype", ColType::Int4),
                    ("amprocnum", ColType::Int4),
                    ("amproc", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_ts_parser") => finish(
            def_of(
                "pg_ts_parser",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("prsname", ColType::Name),
                    ("prsnamespace", ColType::Int4),
                    ("prsstart", ColType::Int4),
                    ("prstoken", ColType::Int4),
                    ("prsend", ColType::Int4),
                    ("prsheadline", ColType::Int4),
                    ("prslextype", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_ts_template") => finish(
            def_of(
                "pg_ts_template",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("tmplname", ColType::Name),
                    ("tmplnamespace", ColType::Int4),
                    ("tmplinit", ColType::Int4),
                    ("tmpllexize", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_ts_dict") => finish(
            def_of(
                "pg_ts_dict",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("dictname", ColType::Name),
                    ("dictnamespace", ColType::Int4),
                    ("dictowner", ColType::Int4),
                    ("dicttemplate", ColType::Int4),
                    ("dictinitoption", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_ts_config") => finish(
            def_of(
                "pg_ts_config",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("cfgname", ColType::Name),
                    ("cfgnamespace", ColType::Int4),
                    ("cfgowner", ColType::Int4),
                    ("cfgparser", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_ts_config_map") => finish(
            def_of(
                "pg_ts_config_map",
                &[
                    ("mapcfg", ColType::Int4),
                    ("maptokentype", ColType::Int4),
                    ("mapseqno", ColType::Int4),
                    ("mapdict", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_init_privs") => finish(
            def_of(
                "pg_init_privs",
                &[
                    ("objoid", ColType::Int4),
                    ("classoid", ColType::Int4),
                    ("objsubid", ColType::Int4),
                    ("privtype", ColType::Bpchar),
                    ("initprivs", ColType::Array(super::types::ArrElem::Text)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_cast") => finish(
            def_of(
                "pg_cast",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("castsource", ColType::Int4),
                    ("casttarget", ColType::Int4),
                    ("castfunc", ColType::Int4),
                    ("castcontext", ColType::Bpchar),
                    ("castmethod", ColType::Bpchar),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_transform") => finish(
            def_of(
                "pg_transform",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("trftype", ColType::Int4),
                    ("trflang", ColType::Int4),
                    ("trffromsql", ColType::Int4),
                    ("trftosql", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_language") => finish(
            def_of(
                "pg_language",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("lanname", ColType::Name),
                    ("lanowner", ColType::Int4),
                    ("lanpltrusted", ColType::Bool),
                    ("lanispl", ColType::Bool),
                    ("lanplcallfoid", ColType::Int4),
                    ("lanvalidator", ColType::Int4),
                    ("laninline", ColType::Int4),
                    ("lanacl", ColType::Array(super::types::ArrElem::Text)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_auth_members") => pg_auth_members(storage, txid, arena),
        (false, "pg_db_role_setting") => finish(
            def_of(
                "pg_db_role_setting",
                &[
                    ("setdatabase", ColType::Int4),
                    ("setrole", ColType::Int4),
                    ("setconfig", ColType::Array(super::types::ArrElem::Text)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_parameter_acl") => finish(
            def_of(
                "pg_parameter_acl",
                &[
                    ("parname", ColType::Text),
                    ("paracl", ColType::Array(super::types::ArrElem::Text)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_default_acl") => pg_default_acl(storage, txid, arena),
        (false, "pg_extension") => finish(
            def_of(
                "pg_extension",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("extname", ColType::Name),
                    ("extnamespace", ColType::Int4),
                    ("extrelocatable", ColType::Bool),
                    ("extversion", ColType::Text),
                    ("extconfig", ColType::Array(super::types::ArrElem::Int4)),
                    ("extcondition", ColType::Array(super::types::ArrElem::Text)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_depend") => pg_depend(storage, txid, arena),
        (false, "pg_tablespace") => {
            let def = def_of(
                "pg_tablespace",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("spcname", ColType::Name),
                    ("spcowner", ColType::Int4),
                    ("spcacl", ColType::Array(super::types::ArrElem::Text)),
                    ("spcoptions", ColType::Array(super::types::ArrElem::Text)),
                ],
            );
            finish(
                def,
                &[row(
                    &[
                        Datum::Int4(1213),
                        Datum::Int4(1663),
                        text("pg_default", arena)?,
                        Datum::Int4(10),
                        Datum::Null,
                        Datum::Null,
                    ],
                    arena,
                )?],
                arena,
            )
        }
        (false, "pg_roles") => pg_roles(storage, txid, arena),
        (false, "pg_authid") => pg_authid(storage, txid, arena),
        (false, "pg_description") => pg_description(storage, txid, arena),
        (false, "pg_seclabels") => finish(
            def_of(
                "pg_seclabels",
                &[
                    ("objoid", ColType::Int4),
                    ("classoid", ColType::Int4),
                    ("objsubid", ColType::Int4),
                    ("objtype", ColType::Text),
                    ("objnamespace", ColType::Int4),
                    ("objname", ColType::Text),
                    ("provider", ColType::Text),
                    ("label", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_shseclabel") => finish(
            def_of(
                "pg_shseclabel",
                &[
                    ("objoid", ColType::Int4),
                    ("classoid", ColType::Int4),
                    ("provider", ColType::Text),
                    ("label", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_largeobject_metadata") => finish(
            def_of(
                "pg_largeobject_metadata",
                &[
                    ("oid", ColType::Int4),
                    ("lomowner", ColType::Int4),
                    ("lomacl", ColType::Array(super::types::ArrElem::Text)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_largeobject") => finish(
            def_of(
                "pg_largeobject",
                &[
                    ("loid", ColType::Int4),
                    ("pageno", ColType::Int4),
                    ("data", ColType::Bytea),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_enum") => pg_enum(storage, txid, arena),
        (false, "pg_range") => finish(
            def_of(
                "pg_range",
                &[
                    ("rngtypid", ColType::Int4),
                    ("rngsubtype", ColType::Int4),
                    ("rngmultitypid", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_matviews") => pg_matviews(storage, txid, arena),
        (false, "pg_sequences") => pg_sequences(storage, txid, arena),
        (false, "pg_sequence") => pg_sequence(storage, txid, arena),
        (false, "pg_database") => finish(
            def_of(
                "pg_database",
                &[
                    ("oid", ColType::Int4),
                    ("datname", ColType::Name),
                    ("datdba", ColType::Int4),
                    ("encoding", ColType::Int4),
                    ("datcollate", ColType::Text),
                    ("datctype", ColType::Text),
                    ("daticulocale", ColType::Text),
                    ("datlocprovider", ColType::Bpchar),
                    ("datacl", ColType::Array(super::types::ArrElem::Text)),
                    ("dattablespace", ColType::Int4),
                ],
            ),
            &[row(
                &[
                    Datum::Int4(5),
                    text("postgres", arena)?,
                    Datum::Int4(10),
                    Datum::Int4(6),
                    text("C", arena)?,
                    text("C", arena)?,
                    Datum::Null,
                    text("c", arena)?,
                    Datum::Null,
                    Datum::Int4(1663),
                ],
                arena,
            )?],
            arena,
        ),
        (false, "pg_views") => pg_views(storage, txid, arena),
        (true, "tables") => info_tables(storage, txid, arena),
        (true, "columns") => info_columns(storage, txid, arena),
        (true, "schemata") => info_schemata(storage, txid, arena),
        (true, "table_constraints") => info_table_constraints(storage, txid, arena),
        (true, "key_column_usage") => info_key_column_usage(storage, txid, arena),
        (true, "constraint_column_usage") => info_constraint_column_usage(storage, txid, arena),
        (true, "referential_constraints") => info_referential_constraints(storage, txid, arena),
        (true, "table_privileges") => info_table_privileges(storage, txid, arena),
        (true, "role_table_grants") => info_role_table_grants(storage, txid, arena),
        (true, "sequences") => info_sequences(storage, txid, arena),
        (true, "usage_privileges") => info_usage_privileges(storage, txid, arena),
        (true, "domains") => info_domains(storage, txid, arena),
        (true, "domain_constraints") => info_domain_constraints(storage, txid, arena),
        (true, "check_constraints") => info_check_constraints(storage, txid, arena),
        (true, "column_domain_usage") => info_column_domain_usage(storage, txid, arena),
        (true, "column_udt_usage") => info_column_udt_usage(storage, txid, arena),
        (true, "domain_udt_usage") => info_domain_udt_usage(storage, txid, arena),
        (true, "collations") => info_collations(arena),
        (true, "collation_character_set_applicability") => {
            info_collation_character_set_applicability(arena)
        }
        (true, "applicable_roles") => info_applicable_roles(storage, txid, arena, false),
        (true, "administrable_role_authorizations") => {
            info_applicable_roles(storage, txid, arena, true)
        }
        (true, "enabled_roles") => info_enabled_roles(storage, txid, arena),
        (true, "column_privileges") => info_column_privileges(storage, txid, arena, true),
        (true, "role_column_grants") => info_column_privileges(storage, txid, arena, false),
        (true, "views") => info_views(storage, txid, arena),
        (true, "routines") => info_routines(storage, txid, arena),
        (true, "parameters") => info_parameters(storage, txid, arena),
        (true, "routine_privileges") => info_routine_privileges(storage, txid, arena, true),
        (true, "role_routine_grants") => info_routine_privileges(storage, txid, arena, false),
        (true, "view_table_usage") => info_view_table_usage(storage, txid, arena),
        (true, "view_column_usage") => info_view_column_usage(storage, txid, arena),
        _ => Err(sql_err!(
            sqlstate::UNDEFINED_TABLE,
            "catalog relation \"{}\" is not implemented",
            name
        )),
    }
}

/// Deterministic oid for a live table: slot index offset into the user
/// range (stable for a running process).
fn table_oid(_storage: &Storage, slot: usize) -> i32 {
    FIRST_USER_OID + slot as i32
}

const PG_DATABASE_OWNER_OID: i32 = 6_171;
const PREDEFINED_ROLES: &[(i32, &str)] = &[(PG_DATABASE_OWNER_OID, "pg_database_owner")];

pub(crate) fn predefined_role_name(oid: i32) -> Option<&'static str> {
    PREDEFINED_ROLES
        .iter()
        .find_map(|(candidate, name)| (*candidate == oid).then_some(*name))
}

#[derive(Clone, Copy)]
enum CatalogOwner {
    Role(usize),
    DatabaseOwner,
}

fn catalog_owner(
    storage: &Storage,
    object: crate::storage::AccessObject,
    txid: u32,
) -> CatalogOwner {
    if matches!(object.class, crate::storage::AccessClass::Schema) && object.slot == 0 {
        CatalogOwner::DatabaseOwner
    } else {
        CatalogOwner::Role(storage.object_owner(object, txid))
    }
}

fn catalog_owner_oid(owner: CatalogOwner) -> i32 {
    match owner {
        CatalogOwner::Role(slot) => Storage::role_oid(slot),
        CatalogOwner::DatabaseOwner => PG_DATABASE_OWNER_OID,
    }
}

fn catalog_owner_name(storage: &Storage, owner: CatalogOwner, txid: u32) -> SqlName {
    match owner {
        CatalogOwner::Role(slot) => storage.role_name(slot, txid),
        CatalogOwner::DatabaseOwner => {
            SqlName::parse("pg_database_owner").expect("built-in role fits")
        }
    }
}

fn owner_oid(storage: &Storage, class: crate::storage::AccessClass, slot: usize, txid: u32) -> i32 {
    catalog_owner_oid(catalog_owner(
        storage,
        crate::storage::AccessObject {
            class,
            slot: slot as u16,
        },
        txid,
    ))
}

fn owner_name<'a>(
    storage: &Storage,
    class: crate::storage::AccessClass,
    slot: usize,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    text(
        catalog_owner_name(
            storage,
            catalog_owner(
                storage,
                crate::storage::AccessObject {
                    class,
                    slot: slot as u16,
                },
                txid,
            ),
            txid,
        )
        .as_str(),
        arena,
    )
}

fn acl<'a>(
    storage: &Storage,
    object: crate::storage::AccessObject,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use core::fmt::Write;
    let owner = storage.object_owner(object, txid);
    let explicit_owner_acl = storage.acl_entries().any(|(slot, entry)| {
        let (grantee, grantor) = storage.acl_identity(slot, txid);
        entry.object == object
            && grantee == owner as u16
            && grantor == owner as u16
            && entry.object.slot != u16::MAX
    });
    let has_entries = storage.acl_entries().any(|(slot, entry)| {
        let (grantee, _) = storage.acl_identity(slot, txid);
        entry.object == object
            && (storage.acl_state(slot, txid).0.0 != 0
                || (explicit_owner_acl && grantee == owner as u16)
                || (matches!(
                    object.class,
                    crate::storage::AccessClass::Domain
                        | crate::storage::AccessClass::Enum
                        | crate::storage::AccessClass::Routine
                ) && grantee == crate::storage::PUBLIC_ROLE))
    });
    if !has_entries {
        return Ok(Datum::Null);
    }
    let mut values = [Datum::Null; crate::storage::MAX_ACL_ENTRIES + 1];
    let owner_name = catalog_owner_name(storage, catalog_owner(storage, object, txid), txid);
    let all = match object.class {
        crate::storage::AccessClass::Table
        | crate::storage::AccessClass::View
        | crate::storage::AccessClass::MaterializedView => crate::storage::PrivilegeSet::TABLE_ALL,
        crate::storage::AccessClass::Sequence => crate::storage::PrivilegeSet::SEQUENCE_ALL,
        crate::storage::AccessClass::Schema => crate::storage::PrivilegeSet::SCHEMA_ALL,
        crate::storage::AccessClass::Domain | crate::storage::AccessClass::Enum => {
            crate::storage::PrivilegeSet::TYPE_ALL
        }
        crate::storage::AccessClass::Index => crate::storage::PrivilegeSet::NONE,
        crate::storage::AccessClass::Routine => crate::storage::PrivilegeSet::FUNCTION_ALL,
    };
    let render = |grantee: &str,
                  grantor: &str,
                  privileges: crate::storage::PrivilegeSet,
                  grant_options: crate::storage::PrivilegeSet,
                  output: &mut StackStr<256>| {
        let _ = write!(output, "{grantee}=");
        let letters = [
            (crate::storage::PrivilegeSet::INSERT, 'a'),
            (crate::storage::PrivilegeSet::SELECT, 'r'),
            (crate::storage::PrivilegeSet::UPDATE, 'w'),
            (crate::storage::PrivilegeSet::DELETE, 'd'),
            (crate::storage::PrivilegeSet::TRUNCATE, 'D'),
            (crate::storage::PrivilegeSet::REFERENCES, 'x'),
            (crate::storage::PrivilegeSet::TRIGGER, 't'),
            (crate::storage::PrivilegeSet::MAINTAIN, 'm'),
            (crate::storage::PrivilegeSet::USAGE, 'U'),
            (crate::storage::PrivilegeSet::CREATE, 'C'),
            (crate::storage::PrivilegeSet::EXECUTE, 'X'),
        ];
        for (privilege, letter) in letters {
            if privileges.contains(privilege) {
                let _ = write!(output, "{letter}");
                if grant_options.contains(privilege) {
                    let _ = write!(output, "*");
                }
            }
        }
        let _ = write!(output, "/{grantor}");
    };
    let mut owner_acl = StackStr::<256>::new();
    let (owner_privileges, owner_options) = if explicit_owner_acl {
        storage.acl_from(object, owner as u16, owner as u16, txid)
    } else {
        (all, crate::storage::PrivilegeSet::NONE)
    };
    render(
        owner_name.as_str(),
        owner_name.as_str(),
        owner_privileges,
        owner_options,
        &mut owner_acl,
    );
    values[0] = Datum::Text(
        arena
            .alloc_str(owner_acl.as_str())
            .map_err(|_| arena_full())?,
    );
    let mut count = 1usize;
    for (slot, entry) in storage.acl_entries() {
        let (grantee, grantor) = storage.acl_identity(slot, txid);
        if entry.object != object || (grantee == owner as u16 && grantor == owner as u16) {
            continue;
        }
        if storage
            .acl_entries()
            .take(slot)
            .any(|(earlier_slot, earlier)| {
                earlier.object == object
                    && storage.acl_identity(earlier_slot, txid) != (owner as u16, owner as u16)
                    && storage.acl_identity(earlier_slot, txid) == (grantee, grantor)
            })
        {
            continue;
        }
        let (privileges, grant_options) = storage.acl_from(object, grantee, grantor, txid);
        if privileges.0 == 0 {
            continue;
        }
        let grantee_name = (grantee != crate::storage::PUBLIC_ROLE)
            .then(|| storage.role_name(grantee as usize, txid));
        let grantee = grantee_name.as_ref().map_or("", |name| name.as_str());
        let grantor_name = if matches!(
            catalog_owner(storage, object, txid),
            CatalogOwner::DatabaseOwner
        ) && grantor == owner as u16
        {
            None
        } else {
            Some(storage.role_name(grantor as usize, txid))
        };
        let grantor = grantor_name
            .as_ref()
            .map_or("pg_database_owner", |name| name.as_str());
        let mut rendered = StackStr::<256>::new();
        render(grantee, grantor, privileges, grant_options, &mut rendered);
        values[count] = Datum::Text(
            arena
                .alloc_str(rendered.as_str())
                .map_err(|_| arena_full())?,
        );
        count += 1;
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Text,
        raw: super::array::build(&values[..count], arena)?,
    })
}

fn pg_default_acl<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    use crate::storage::{
        DEFAULT_ACL_ALL_SCHEMAS, DefaultPrivilegeClass, MAX_DEFAULT_ACL_ENTRIES, MAX_ROLES,
        PUBLIC_ROLE, PrivilegeSet,
    };
    use core::fmt::Write;

    let def = def_of(
        "pg_default_acl",
        &[
            ("oid", ColType::Int4),
            ("tableoid", ColType::Int4),
            ("defaclrole", ColType::Int4),
            ("defaclnamespace", ColType::Int4),
            ("defaclobjtype", ColType::Bpchar),
            ("defaclacl", ColType::Array(super::types::ArrElem::Text)),
        ],
    );
    let mut rows: [&[Datum]; MAX_DEFAULT_ACL_ENTRIES] = [&[]; MAX_DEFAULT_ACL_ENTRIES];
    let mut row_count = 0usize;

    for (entry_slot, entry) in storage.default_acl_entries() {
        let (defined, _, _) =
            storage.default_acl_state(entry.owner, entry.schema, entry.class, entry.grantee, txid);
        if !defined
            || storage
                .default_acl_entries()
                .take(entry_slot)
                .any(|(_, earlier)| {
                    let (earlier_defined, _, _) = storage.default_acl_state(
                        earlier.owner,
                        earlier.schema,
                        earlier.class,
                        earlier.grantee,
                        txid,
                    );
                    earlier_defined
                        && earlier.owner == entry.owner
                        && earlier.schema == entry.schema
                        && earlier.class == entry.class
                })
        {
            continue;
        }
        let mut acl_values = [Datum::Null; MAX_ROLES + 1];
        let mut acl_count = 0usize;
        for role_index in 0..=MAX_ROLES {
            let grantee = if role_index == MAX_ROLES {
                PUBLIC_ROLE
            } else {
                if !storage.role(role_index).visible_to(txid) {
                    continue;
                }
                role_index as u16
            };
            let (explicit, privileges, grant_options) =
                storage.default_acl_state(entry.owner, entry.schema, entry.class, grantee, txid);
            let (privileges, grant_options) = if explicit {
                (privileges, grant_options)
            } else if entry.schema == DEFAULT_ACL_ALL_SCHEMAS {
                Storage::default_acl_baseline(entry.owner, entry.schema, entry.class, grantee)
            } else {
                (PrivilegeSet::NONE, PrivilegeSet::NONE)
            };
            let include = if entry.schema == DEFAULT_ACL_ALL_SCHEMAS {
                grantee == entry.owner
                    || explicit
                    || (grantee == PUBLIC_ROLE && entry.class.default_public_privileges().0 != 0)
            } else {
                explicit
            };
            if !include {
                continue;
            }
            let named_grantee =
                (grantee != PUBLIC_ROLE).then(|| storage.role_name(grantee as usize, txid));
            let grantee_name = named_grantee.as_ref().map_or("", SqlName::as_str);
            let owner_name = storage.role_name(entry.owner as usize, txid);
            let mut rendered = StackStr::<256>::new();
            let _ = write!(rendered, "{grantee_name}=");
            for (privilege, letter) in [
                (PrivilegeSet::SELECT, 'r'),
                (PrivilegeSet::INSERT, 'a'),
                (PrivilegeSet::UPDATE, 'w'),
                (PrivilegeSet::DELETE, 'd'),
                (PrivilegeSet::TRUNCATE, 'D'),
                (PrivilegeSet::REFERENCES, 'x'),
                (PrivilegeSet::TRIGGER, 't'),
                (PrivilegeSet::MAINTAIN, 'm'),
                (PrivilegeSet::USAGE, 'U'),
                (PrivilegeSet::CREATE, 'C'),
                (PrivilegeSet::EXECUTE, 'X'),
            ] {
                if privileges.contains(privilege) {
                    let _ = write!(rendered, "{letter}");
                    if grant_options.contains(privilege) {
                        let _ = write!(rendered, "*");
                    }
                }
            }
            let _ = write!(rendered, "/{}", owner_name.as_str());
            acl_values[acl_count] = text(rendered.as_str(), arena)?;
            acl_count += 1;
        }
        let namespace = if entry.schema == DEFAULT_ACL_ALL_SCHEMAS {
            0
        } else {
            namespace_oid(
                storage,
                storage.schema_def(entry.schema as usize).name.as_str(),
            )
        };
        let object_type = match entry.class {
            DefaultPrivilegeClass::Table => "r",
            DefaultPrivilegeClass::Sequence => "S",
            DefaultPrivilegeClass::Function => "f",
            DefaultPrivilegeClass::Type => "T",
            DefaultPrivilegeClass::Schema => "n",
        };
        rows[row_count] = row(
            &[
                Datum::Int4(90_000 + entry_slot as i32),
                Datum::Int4(826),
                Datum::Int4(Storage::role_oid(entry.owner as usize)),
                Datum::Int4(namespace),
                text(object_type, arena)?,
                Datum::Array {
                    element: super::types::ArrElem::Text,
                    raw: super::array::build(&acl_values[..acl_count], arena)?,
                },
            ],
            arena,
        )?;
        row_count += 1;
    }
    finish(def, &rows[..row_count], arena)
}

/// Schema OIDs: the two built-ins keep PostgreSQL's well-known values; a user
/// schema's OID is derived from its registry slot, above the table range.
const FIRST_SCHEMA_OID: i32 = 80_000;
fn namespace_oid(storage: &Storage, schema: &str) -> i32 {
    match schema {
        "public" => PUBLIC_NS_OID,
        "pg_catalog" => PG_CATALOG_NS_OID,
        _ => storage
            .find_schema(schema)
            .map(|slot| FIRST_SCHEMA_OID + slot as i32)
            .unwrap_or(0),
    }
}

pub(crate) fn schema_name_by_oid(storage: &Storage, txid: u32, oid: i32) -> Option<&str> {
    match oid {
        PUBLIC_NS_OID => Some("public"),
        PG_CATALOG_NS_OID => Some("pg_catalog"),
        _ => storage
            .visible_schemas(txid)
            .find(|(_, schema)| namespace_oid(storage, schema.name.as_str()) == oid)
            .map(|(_, schema)| schema.name.as_str()),
    }
}

/// Index relations get OIDs from a separate range so they never collide with
/// table OIDs; `pos` is the index's position within its table's index list.
const FIRST_INDEX_OID: i32 = 90_000;
const MAX_INDEXES_PER_TABLE: i32 = 64;
fn index_oid(slot: usize, pos: usize) -> i32 {
    FIRST_INDEX_OID + slot as i32 * MAX_INDEXES_PER_TABLE + pos as i32
}

/// Sequence relations get OIDs from their own range, above the index range.
const FIRST_SEQUENCE_OID: i32 = 95_000;
fn sequence_oid(slot: usize) -> i32 {
    FIRST_SEQUENCE_OID + slot as i32
}

pub(crate) fn sequence_state_by_oid(storage: &Storage, oid: i32) -> Option<(i64, bool)> {
    storage
        .sequences_with_slots()
        .find(|(slot, _)| sequence_oid(*slot) == oid)
        .map(|(_, sequence)| (sequence.last_value.get(), sequence.is_called.get()))
}

/// Plain views get OIDs from their own range so `'view'::regclass` resolves and
/// their comments surface, even though a view is not yet a full `pg_class` row.
const FIRST_VIEW_OID: i32 = 100_000;
fn view_oid(slot: usize) -> i32 {
    FIRST_VIEW_OID + slot as i32
}

/// PostgreSQL gives each view's `_RETURN` rule a catalog identity distinct
/// from the view relation. Keeping the mapping slot-based makes rule and
/// dependency rows survive rename, checkpoint, recovery, and replacement.
const FIRST_VIEW_REWRITE_OID: i32 = 110_000;
fn view_rewrite_oid(slot: usize) -> i32 {
    FIRST_VIEW_REWRITE_OID + slot as i32
}

fn domain_oid(slot: usize) -> i32 {
    crate::sql::types::oid::domain_oid(slot as u16)
}

/// Tables/materialized views and plain views have distinct composite-type OID
/// bands. PostgreSQL gives every row-bearing relation a separate pg_type row.
const FIRST_TABLE_COMPOSITE_TYPE_OID: i32 = 130_000;
const FIRST_VIEW_COMPOSITE_TYPE_OID: i32 = 140_000;

fn composite_type_oid(storage: &Storage, schema: &str, name: &str, txid: u32) -> Option<i32> {
    match storage.resolve_relation(Some(schema), name, txid)? {
        crate::storage::ResolvedRelation::Table(slot) => {
            Some(FIRST_TABLE_COMPOSITE_TYPE_OID + slot as i32)
        }
        crate::storage::ResolvedRelation::View(slot) => {
            Some(FIRST_VIEW_COMPOSITE_TYPE_OID + slot as i32)
        }
        crate::storage::ResolvedRelation::Catalog => None,
    }
}

/// One materialized index relation (implicit primary-key / unique index from a
/// constraint, or an explicit `CREATE INDEX`).
#[derive(Clone, Copy)]
struct IdxInfo {
    oid: i32,
    table_oid: i32,
    table_slot: usize,
    name: StackStr<64>,
    columns: [u16; crate::storage::MAX_INDEX_COLS],
    descending: [bool; crate::storage::MAX_INDEX_COLS],
    nulls_first: [bool; crate::storage::MAX_INDEX_COLS],
    n_cols: usize,
    is_primary: bool,
    is_unique: bool,
}

/// Enumerates every index relation psql `\d` would show: a single-column PK or
/// UNIQUE (from column flags), a multi-column PK/UNIQUE (from `uniques`), and
/// explicit `CREATE INDEX`es. OIDs are assigned by table slot + position so the
/// same index resolves identically here and in `pg_get_indexdef`.
fn visit_indexes(storage: &Storage, txid: u32, mut visit: impl FnMut(IdxInfo)) {
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let def = storage.table_def(slot, txid);
        let table_name = def.name.as_str();
        let toid = table_oid(storage, slot);
        let mut pos = 0usize;
        let mut mk = |columns: &[u16],
                      descending: [bool; crate::storage::MAX_INDEX_COLS],
                      nulls_first: [bool; crate::storage::MAX_INDEX_COLS],
                      is_primary: bool,
                      is_unique: bool,
                      name: StackStr<64>| {
            let mut c = [0u16; crate::storage::MAX_INDEX_COLS];
            c[..columns.len()].copy_from_slice(columns);
            let info = IdxInfo {
                oid: index_oid(slot, pos),
                table_oid: toid,
                table_slot: slot,
                name,
                columns: c,
                descending,
                nulls_first,
                n_cols: columns.len(),
                is_primary,
                is_unique,
            };
            pos += 1;
            info
        };
        // Single-column PK / UNIQUE carried as column flags.
        for (ci, col) in def.columns().iter().enumerate() {
            if col.primary {
                let name = stack_str_64(stack_format!(64, "{}_pkey", table_name).as_str());
                visit(mk(
                    &[ci as u16],
                    [false; crate::storage::MAX_INDEX_COLS],
                    [false; crate::storage::MAX_INDEX_COLS],
                    true,
                    true,
                    name,
                ));
            } else if col.unique {
                let name = stack_str_64(
                    stack_format!(64, "{}_{}_key", table_name, col.name.as_str()).as_str(),
                );
                visit(mk(
                    &[ci as u16],
                    [false; crate::storage::MAX_INDEX_COLS],
                    [false; crate::storage::MAX_INDEX_COLS],
                    false,
                    true,
                    name,
                ));
            }
        }
        // Multi-column PK / UNIQUE constraints.
        for uk in def.uniques() {
            visit(mk(
                uk.columns(),
                [false; crate::storage::MAX_INDEX_COLS],
                [false; crate::storage::MAX_INDEX_COLS],
                uk.is_primary,
                true,
                stack_str_64(uk.name.as_str()),
            ));
        }
        // Explicit CREATE INDEX on this table.
        for index in storage.indexes_for(def.schema.as_str(), table_name, txid) {
            visit(mk(
                &index.columns[..index.n_cols],
                index.descending,
                index.nulls_first,
                false,
                index.unique,
                stack_str_64(index.name.as_str()),
            ));
        }
    }
}

fn empty_index() -> IdxInfo {
    IdxInfo {
        oid: 0,
        table_oid: 0,
        table_slot: 0,
        name: StackStr::new(),
        columns: [0; crate::storage::MAX_INDEX_COLS],
        descending: [false; crate::storage::MAX_INDEX_COLS],
        nulls_first: [false; crate::storage::MAX_INDEX_COLS],
        n_cols: 0,
        is_primary: false,
        is_unique: false,
    }
}

/// Materializes every visible index in the statement arena. This is sized from
/// the actual catalog, rather than a separate arbitrary limit, so an accepted
/// startup table/index capacity cannot silently disappear from introspection.
fn collect_indexes<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [IdxInfo], SqlError> {
    let mut count = 0usize;
    visit_indexes(storage, txid, |_| count += 1);
    let indexes = arena
        .alloc_slice_with(count, |_| empty_index())
        .map_err(|_| arena_full())?;
    let mut next = 0usize;
    visit_indexes(storage, txid, |index| {
        indexes[next] = index;
        next += 1;
    });
    Ok(indexes)
}

fn index_oid_by_name(
    storage: &Storage,
    txid: u32,
    schema: Option<&str>,
    name: &str,
) -> Option<i32> {
    let mut found = None;
    visit_indexes(storage, txid, |index| {
        if found.is_none()
            && index.name.as_str() == name
            && schema.is_none_or(|schema| {
                storage.table_def(index.table_slot, txid).schema.as_str() == schema
            })
        {
            found = Some(index.oid);
        }
    });
    found
}

fn has_index_oid(storage: &Storage, txid: u32, oid: i32) -> bool {
    let mut found = false;
    visit_indexes(storage, txid, |index| found |= index.oid == oid);
    found
}

fn stack_str_64(s: &str) -> StackStr<64> {
    let mut out = StackStr::<64>::new();
    let _ = core::fmt::Write::write_str(&mut out, s);
    out
}

/// The relation name for an OID, used to render `oid::regclass`. Resolves
/// ordinary tables, synthesized index relations, sequences and plain views.
pub fn relname_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    for name in [
        "pg_type",
        "pg_proc",
        "pg_class",
        "pg_attribute",
        "pg_amop",
        "pg_amproc",
        "pg_cast",
        "pg_constraint",
        "pg_depend",
        "pg_rewrite",
        "pg_namespace",
        "pg_opfamily",
        "pg_extension",
        "pg_transform",
    ] {
        if catalog_relation_oid(name) == Some(oid) {
            return arena.alloc_str(name).map(Some).map_err(|_| arena_full());
        }
    }
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        if table_oid(storage, slot) == oid {
            let bytes = arena
                .alloc_slice_copy(storage.table_def(slot, txid).name.as_str().as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    let indices = collect_indexes(storage, txid, arena)?;
    for info in indices {
        if info.oid == oid {
            let bytes = arena
                .alloc_slice_copy(info.name.as_str().as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    for slot in 0..storage.sequence_count() {
        let seq = storage.sequence_for(slot, txid);
        if !seq.visible_to(txid) {
            continue;
        }
        if sequence_oid(slot) == oid {
            let bytes = arena
                .alloc_slice_copy(seq.name.as_str().as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    for slot in 0..storage.view_count() {
        let view = storage.view(slot);
        if !view.visible_to(txid) {
            continue;
        }
        if view_oid(slot) == oid {
            let bytes = arena
                .alloc_slice_copy(view.name.as_str().as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    Ok(None)
}

/// The OID of the relation named `name`, for `'relname'::regclass`. Resolves
/// ordinary tables, synthesized index relations and sequences; `None` if no
/// such relation.
pub fn reloid_of_name(storage: &Storage, txid: u32, name: &str) -> Option<i32> {
    let (schema, relation) = name
        .split_once('.')
        .map_or((None, name), |(schema, relation)| (Some(schema), relation));
    if schema.is_none_or(|schema| schema == "pg_catalog")
        && let Some(oid) = catalog_relation_oid(relation)
    {
        return Some(oid);
    }
    for slot in 0..storage.table_count() {
        if storage.table(slot).visible_to(txid)
            && storage.table_def(slot, txid).name.as_str() == relation
            && schema.is_none_or(|schema| storage.table_def(slot, txid).schema.as_str() == schema)
        {
            return Some(table_oid(storage, slot));
        }
    }
    if let Some(oid) = index_oid_by_name(storage, txid, schema, relation) {
        return Some(oid);
    }
    for slot in 0..storage.sequence_count() {
        let sequence = storage.sequence_for(slot, txid);
        if sequence.visible_to(txid)
            && sequence.name.as_str() == relation
            && schema.is_none_or(|schema| sequence.schema.as_str() == schema)
        {
            return Some(sequence_oid(slot));
        }
    }
    (0..storage.view_count())
        .find(|&slot| {
            storage.view(slot).visible_to(txid)
                && storage.view(slot).name.as_str() == relation
                && schema.is_none_or(|schema| storage.view(slot).schema.as_str() == schema)
        })
        .map(view_oid)
}

/// Catalog identity checks used by PostgreSQL's visibility helpers.  The
/// synthesized catalogs and executable SQL built-ins deliberately have
/// separate namespaces, so an OID is accepted only by the predicate that
/// owns it.
pub fn relation_oid_is_visible(storage: &Storage, txid: u32, oid: i32) -> bool {
    catalog_relation_oid_by_oid(oid)
        || (0..storage.table_count())
            .any(|slot| storage.table(slot).visible_to(txid) && table_oid(storage, slot) == oid)
        || has_index_oid(storage, txid, oid)
        || (0..storage.sequence_count()).any(|slot| {
            storage.sequence_for(slot, txid).visible_to(txid) && sequence_oid(slot) == oid
        })
        || (0..storage.view_count())
            .any(|slot| storage.view(slot).visible_to(txid) && view_oid(slot) == oid)
}

fn catalog_relation_oid_by_oid(oid: i32) -> bool {
    [
        "pg_type",
        "pg_proc",
        "pg_class",
        "pg_attribute",
        "pg_amop",
        "pg_amproc",
        "pg_cast",
        "pg_constraint",
        "pg_depend",
        "pg_rewrite",
        "pg_namespace",
        "pg_opfamily",
        "pg_extension",
        "pg_default_acl",
        "pg_replication_slots",
        "pg_transform",
    ]
    .into_iter()
    .any(|name| catalog_relation_oid(name) == Some(oid))
}

pub fn type_oid_is_visible(storage: &Storage, txid: u32, oid: i32) -> bool {
    if super::types::ColType::from_oid(oid).is_some()
        || matches!(
            oid,
            26 | 2249 | 2202 | 2203 | 2204 | 2205 | 2206 | 4096 | 4097
        )
    {
        return true;
    }
    use super::types::oid as type_oid;
    let visible_slot = |first, count, visible: &dyn Fn(usize) -> bool| {
        (first..first + count as i32).contains(&oid) && visible((oid - first) as usize)
    };
    visible_slot(
        type_oid::FIRST_DOMAIN,
        crate::storage::MAX_DOMAINS,
        &|slot| storage.domain(slot).visible_to(txid),
    ) || visible_slot(type_oid::FIRST_ENUM, crate::storage::MAX_ENUMS, &|slot| {
        storage.enum_for(slot, txid).visible_to(txid)
    })
}

pub fn function_oid_is_visible(oid: i32) -> bool {
    INTRINSIC_ROUTINES.iter().any(|routine| routine.oid == oid)
}

pub fn function_def_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let Some(slot) = storage.routine_slot_by_oid(oid, txid) else {
        return Ok(None);
    };
    let routine = storage.routine(slot);
    use core::fmt::Write;
    let mut definition = crate::util::StackStr::<{ crate::storage::ROUTINE_SQL_MAX * 2 }>::new();
    write!(
        definition,
        "CREATE OR REPLACE {} {}.{}(",
        if matches!(routine.kind, crate::storage::RoutineKind::Procedure) {
            "PROCEDURE"
        } else {
            "FUNCTION"
        },
        routine.schema_for(txid).as_str(),
        routine.name_for(txid).as_str()
    )
    .map_err(|_| super::eval::arena_full())?;
    for (index, argument) in routine.arguments().iter().enumerate() {
        if index != 0 {
            write!(definition, ", ").map_err(|_| super::eval::arena_full())?;
        }
        if !argument.name.as_str().is_empty() {
            write!(definition, "{} ", argument.name.as_str())
                .map_err(|_| super::eval::arena_full())?;
        }
        write!(definition, "{}", argument.ctype.name()).map_err(|_| super::eval::arena_full())?;
    }
    match routine.kind {
        crate::storage::RoutineKind::Function { result } => {
            write!(definition, ") RETURNS {} LANGUAGE sql AS '", result.name())
        }
        crate::storage::RoutineKind::Procedure => write!(definition, ") LANGUAGE sql AS '"),
    }
    .map_err(|_| super::eval::arena_full())?;
    for character in routine.body.as_str().chars() {
        write!(definition, "{character}").map_err(|_| super::eval::arena_full())?;
        if character == '\'' {
            write!(definition, "'").map_err(|_| super::eval::arena_full())?;
        }
    }
    write!(definition, "'").map_err(|_| super::eval::arena_full())?;
    if definition.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "routine definition exceeds catalog rendering capacity"
        ));
    }
    Ok(Some(
        arena
            .alloc_str(definition.as_str())
            .map_err(|_| super::eval::arena_full())?,
    ))
}

pub fn collation_oid_is_visible(oid: i32) -> bool {
    matches!(oid, 100 | 950 | 951 | 12_340)
}

pub fn relation_oid_is_publishable(storage: &Storage, txid: u32, oid: i32) -> bool {
    (0..storage.table_count())
        .any(|slot| storage.table(slot).visible_to(txid) && table_oid(storage, slot) == oid)
}

/// Stored SELECT text for `pg_get_viewdef`, by relation OID.
pub fn view_def_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    for slot in 0..storage.table_count() {
        let table = storage.table(slot);
        if !table.visible_to(txid) {
            continue;
        }
        if table_oid(storage, slot) != oid {
            continue;
        }
        let Some(view) =
            storage.find_matview(table.def.schema.as_str(), table.def.name.as_str(), txid)
        else {
            return Ok(None);
        };
        return arena
            .alloc_str_display(format_args!("{};", view.sql.as_str()))
            .map(Some)
            .map_err(|_| arena_full());
    }
    for (slot, view) in storage.views_visible_to(txid) {
        if view_oid(slot) == oid {
            return arena
                .alloc_str_display(format_args!("{};", view.sql.as_str()))
                .map(Some)
                .map_err(|_| arena_full());
        }
    }
    Ok(None)
}

/// The bytes occupied by a relation's visible encoded row images. Plain views
/// and indexes have no physical row store in pos3ql, so their exact size is
/// zero; tables and materialized views share the table row store.
pub fn relation_size(storage: &Storage, txid: u32, oid: i32) -> Result<Option<i64>, SqlError> {
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        if table_oid(storage, slot) != oid {
            continue;
        }
        return table_size(storage, txid, slot).map(Some);
    }
    if storage
        .views_visible_to(txid)
        .any(|(slot, _)| view_oid(slot) == oid)
    {
        return Ok(Some(0));
    }
    if has_index_oid(storage, txid, oid) {
        return Ok(Some(0));
    }
    Ok(None)
}

pub fn database_size(storage: &Storage, txid: u32) -> Result<i64, SqlError> {
    let mut total = 0i64;
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        total = total
            .checked_add(table_size(storage, txid, slot)?)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::NUMERIC_OUT_OF_RANGE,
                    "database size exceeds bigint"
                )
            })?;
    }
    Ok(total)
}

fn table_size(storage: &Storage, txid: u32, slot: usize) -> Result<i64, SqlError> {
    let mut bytes = 0i64;
    storage.for_each_row_state(slot, &mut |rowid, state| {
        if let Some(home) = storage.visible_row_home(slot, rowid, state, txid)? {
            let len = match home {
                crate::storage::RowHome::Heap(location) => location.len,
                crate::storage::RowHome::Spilled { len, .. } => len,
            };
            bytes = bytes.checked_add(i64::from(len)).ok_or_else(|| {
                sql_err!(
                    sqlstate::NUMERIC_OUT_OF_RANGE,
                    "relation size exceeds bigint"
                )
            })?;
        }
        Ok(core::ops::ControlFlow::Continue(()))
    })?;
    Ok(bytes)
}

/// `pg_description`: one row per committed object comment, resolving each to
/// its `(objoid, classoid, objsubid)`. A comment whose object no longer
/// resolves to an OID is omitted, matching that its row would be dangling.
fn pg_description<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_description",
        &[
            ("objoid", ColType::Int4),
            ("classoid", ColType::Int4),
            ("objsubid", ColType::Int4),
            ("description", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; crate::storage::MAX_COMMENTS] = [&[]; crate::storage::MAX_COMMENTS];
    let mut n = 0;
    for (class, schema, name, subid, description) in storage.comments_visible(txid) {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_description"));
        }
        let (objoid, classoid) = match class {
            crate::storage::CommentClass::Relation => {
                match relation_oid_of(storage, txid, schema, name) {
                    Some(oid) => (oid, PG_CLASS_OID),
                    None => continue,
                }
            }
            crate::storage::CommentClass::Schema => {
                (namespace_oid(storage, name), PG_NAMESPACE_OID)
            }
            crate::storage::CommentClass::Type => match type_oid_of(storage, schema, name, txid) {
                Some(oid) => (oid, PG_TYPE_OID),
                None => continue,
            },
        };
        out[n] = row(
            &[
                Datum::Int4(objoid),
                Datum::Int4(classoid),
                Datum::Int4(subid as i32),
                text(description, arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

/// The `pg_class` OID of a relation named `name` in `schema`: an ordinary
/// table or materialized-view backing table, a sequence, a plain view, or an
/// index.
fn relation_oid_of(storage: &Storage, txid: u32, schema: &str, name: &str) -> Option<i32> {
    if let Some(slot) = storage.find_visible(schema, name, txid) {
        return Some(table_oid(storage, slot));
    }
    for slot in 0..storage.table_count() {
        let table = storage.table(slot);
        if table.visible_to(txid)
            && table.def.schema.as_str() == schema
            && table.def.name.as_str() == name
        {
            return Some(table_oid(storage, slot));
        }
    }
    if let Some(slot) = storage.sequence_slot(schema, name, txid) {
        return Some(sequence_oid(slot));
    }
    for slot in 0..storage.view_count() {
        let view = storage.view(slot);
        if view.visible_to(txid) && view.schema.as_str() == schema && view.name.as_str() == name {
            return Some(view_oid(slot));
        }
    }
    index_oid_by_name(storage, txid, Some(schema), name)
}

/// Resolves a modeled built-in SQL type to its real catalog name and OID.
/// Qualified names accept only `pg_type.typname` spellings; PostgreSQL does
/// not resolve aliases such as `pg_catalog.integer`.
pub fn builtin_type_identity(name: &str, allow_aliases: bool) -> Option<(&'static str, i32)> {
    use crate::sql::types::{ArrElem, oid};

    let catalog_only = match name {
        "oid" => Some(("oid", 26)),
        "record" => Some(("record", oid::RECORD)),
        "regproc" => Some(("regproc", oid::REGPROC)),
        "regprocedure" => Some(("regprocedure", oid::REGPROCEDURE)),
        "regoper" => Some(("regoper", oid::REGOPER)),
        "regoperator" => Some(("regoperator", oid::REGOPERATOR)),
        "regclass" => Some(("regclass", oid::REGCLASS)),
        "regtype" => Some(("regtype", oid::REGTYPE)),
        "regnamespace" => Some(("regnamespace", oid::REGNAMESPACE)),
        "regrole" => Some(("regrole", oid::REGROLE)),
        _ => None,
    };
    if catalog_only.is_some() {
        return catalog_only;
    }
    // SERIAL spellings are DDL shorthand, not entries in pg_type.
    if matches!(
        name,
        "serial" | "serial2" | "serial4" | "serial8" | "smallserial" | "bigserial"
    ) {
        return None;
    }
    let array_elements = [
        ArrElem::Bool,
        ArrElem::Int2,
        ArrElem::Int4,
        ArrElem::Int8,
        ArrElem::Float4,
        ArrElem::Float8,
        ArrElem::Text,
        ArrElem::Name,
        ArrElem::Varchar,
        ArrElem::Bpchar,
        ArrElem::Date,
        ArrElem::Timestamp,
        ArrElem::Timestamptz,
        ArrElem::Time,
        ArrElem::Timetz,
        ArrElem::Interval,
        ArrElem::Json,
        ArrElem::Jsonb,
        ArrElem::Uuid,
        ArrElem::Bytea,
        ArrElem::Numeric,
        ArrElem::Inet,
        ArrElem::Cidr,
        ArrElem::Macaddr,
        ArrElem::Macaddr8,
    ];
    if let Some(element) = array_elements
        .iter()
        .find(|element| element.catalog_name() == name)
    {
        return Some((element.catalog_name(), element.array_oid()));
    }
    let column_type = ColType::from_sql_name(name)?;
    let canonical = column_type.catalog_name();
    if allow_aliases || name == canonical {
        return Some((canonical, column_type.oid()));
    }
    if let (ColType::Array(element), Some(base)) = (column_type, name.strip_suffix("[]"))
        && ColType::from_sql_name(base).is_some_and(|base_type| base_type.catalog_name() == base)
    {
        return Some((element.catalog_name(), element.array_oid()));
    }
    None
}

fn type_oid_of(storage: &Storage, schema: &str, name: &str, txid: u32) -> Option<i32> {
    if let Some(slot) = storage.domain_slot(schema, name, txid) {
        return Some(domain_oid(slot));
    }
    if let Some(slot) = storage.enum_slot(schema, name, txid) {
        return Some(crate::sql::types::oid::enum_oid(slot as u16));
    }
    if let Some(oid) = composite_type_oid(storage, schema, name, txid) {
        return Some(oid);
    }
    if schema == "pg_catalog" {
        return builtin_type_identity(name, false).map(|(_, oid)| oid);
    }
    None
}

/// Search-path-aware SQL spelling of a user-defined type OID. `format_type`
/// uses this for domains/enums and their automatically-created array types;
/// built-in types deliberately return `None` and stay on the static path.
pub fn user_type_name_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    use crate::sql::types::oid as type_oid;
    let (schema, name, visible, array, enumeration) = if (type_oid::FIRST_DOMAIN
        ..type_oid::FIRST_DOMAIN + crate::storage::MAX_DOMAINS as i32)
        .contains(&oid)
    {
        let slot = (oid - type_oid::FIRST_DOMAIN) as usize;
        let definition = storage.domain(slot);
        (
            definition.schema,
            definition.name,
            definition.visible_to(txid),
            false,
            false,
        )
    } else if (type_oid::FIRST_DOMAIN_ARRAY
        ..type_oid::FIRST_DOMAIN_ARRAY + crate::storage::MAX_DOMAINS as i32)
        .contains(&oid)
    {
        let slot = (oid - type_oid::FIRST_DOMAIN_ARRAY) as usize;
        let definition = storage.domain(slot);
        (
            definition.schema,
            definition.name,
            definition.visible_to(txid),
            true,
            false,
        )
    } else if (type_oid::FIRST_ENUM..type_oid::FIRST_ENUM + crate::storage::MAX_ENUMS as i32)
        .contains(&oid)
    {
        let slot = (oid - type_oid::FIRST_ENUM) as usize;
        let definition = storage.enum_for(slot, txid);
        (
            definition.schema,
            definition.name,
            definition.visible_to(txid),
            false,
            true,
        )
    } else if (type_oid::FIRST_ENUM_ARRAY
        ..type_oid::FIRST_ENUM_ARRAY + crate::storage::MAX_ENUMS as i32)
        .contains(&oid)
    {
        let slot = (oid - type_oid::FIRST_ENUM_ARRAY) as usize;
        let definition = storage.enum_for(slot, txid);
        (
            definition.schema,
            definition.name,
            definition.visible_to(txid),
            true,
            true,
        )
    } else {
        return Ok(None);
    };
    if !visible {
        return Ok(None);
    }
    let unqualified_visible = if enumeration {
        storage.resolve_enum_slot(name.as_str(), txid)
            == Some(if array {
                (oid - type_oid::FIRST_ENUM_ARRAY) as usize
            } else {
                (oid - type_oid::FIRST_ENUM) as usize
            })
    } else {
        storage.resolve_domain_slot(name.as_str(), txid)
            == Some(if array {
                (oid - type_oid::FIRST_DOMAIN_ARRAY) as usize
            } else {
                (oid - type_oid::FIRST_DOMAIN) as usize
            })
    };
    let mut rendered = StackStr::<140>::new();
    use core::fmt::Write as _;
    if !unqualified_visible {
        let _ = write!(rendered, "{}.", schema.as_str());
    }
    let _ = rendered.write_str(name.as_str());
    if array {
        let _ = rendered.write_str("[]");
    }
    Ok(Some(alloc_rendered(
        &rendered,
        "formatted type name is too long",
        arena,
    )?))
}

/// The comment text `txid` sees on the object with this OID (and column
/// `subid`), for `obj_description`/`col_description`. `catalog_name` selects
/// the object's catalog, defaulting to `pg_class` for deprecated one-argument
/// `obj_description`.
pub fn comment_text_for<'a>(
    storage: &Storage,
    txid: u32,
    catalog_name: &str,
    oid: i32,
    subid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    for (class, schema, name, csub, text) in storage.comments_visible(txid) {
        let hit = match catalog_name {
            "pg_namespace" => {
                class == crate::storage::CommentClass::Schema
                    && subid == 0
                    && namespace_oid(storage, name) == oid
            }
            "pg_type" => {
                class == crate::storage::CommentClass::Type
                    && subid == 0
                    && type_oid_of(storage, schema, name, txid) == Some(oid)
            }
            _ => {
                class == crate::storage::CommentClass::Relation
                    && csub as i32 == subid
                    && relation_oid_of(storage, txid, schema, name) == Some(oid)
            }
        };
        if hit {
            let bytes = arena
                .alloc_slice_copy(text.as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    Ok(None)
}

/// One materialized foreign-key constraint.
#[derive(Clone, Copy)]
struct FkInfo {
    oid: i32,
    conrelid: i32,
    confrelid: i32,
    child_slot: usize,
    fk_index: usize,
    name: StackStr<64>,
}

const FIRST_FK_OID: i32 = 200_000;
const FIRST_CHECK_OID: i32 = 300_000;
const FIRST_DOMAIN_CHECK_OID: i32 = 400_000;
const FIRST_NOT_NULL_OID: i32 = 450_000;

/// Enumerates every foreign-key constraint, resolving each child/parent table to
/// its OID. A child whose parent no longer exists is skipped (it cannot be
/// rendered), matching that a dropped parent leaves no referential row.
fn visit_fkeys(storage: &Storage, txid: u32, mut visit: impl FnMut(FkInfo)) {
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let def = storage.table_def(slot, txid);
        let conrelid = table_oid(storage, slot);
        for (i, fk) in def.fkeys().iter().enumerate() {
            let Some(pslot) =
                storage.find_visible(fk.parent_schema.as_str(), fk.parent.as_str(), txid)
            else {
                continue;
            };
            visit(FkInfo {
                oid: FIRST_FK_OID + slot as i32 * MAX_INDEXES_PER_TABLE + i as i32,
                conrelid,
                confrelid: table_oid(storage, pslot),
                child_slot: slot,
                fk_index: i,
                name: stack_str_64(fk.name.as_str()),
            });
        }
    }
}

fn empty_foreign_key() -> FkInfo {
    FkInfo {
        oid: 0,
        conrelid: 0,
        confrelid: 0,
        child_slot: 0,
        fk_index: 0,
        name: StackStr::new(),
    }
}

fn collect_fkeys<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [FkInfo], SqlError> {
    let mut count = 0usize;
    visit_fkeys(storage, txid, |_| count += 1);
    let foreign_keys = arena
        .alloc_slice_with(count, |_| empty_foreign_key())
        .map_err(|_| arena_full())?;
    let mut next = 0usize;
    visit_fkeys(storage, txid, |foreign_key| {
        foreign_keys[next] = foreign_key;
        next += 1;
    });
    Ok(foreign_keys)
}

/// The `FOREIGN KEY (...) REFERENCES parent(...)` definition psql prints from
/// `pg_get_constraintdef` for a foreign-key constraint OID.
pub fn constraint_def_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let fks = collect_fkeys(storage, txid, arena)?;
    for info in fks {
        if info.oid != oid {
            continue;
        }
        let child = storage.table_def(info.child_slot, txid);
        let fk = &child.fkeys()[info.fk_index];
        let Some(pslot) = storage.find_visible(fk.parent_schema.as_str(), fk.parent.as_str(), txid)
        else {
            return Ok(None);
        };
        let parent = storage.table_def(pslot, txid);
        let mut s = StackStr::<1280>::new();
        use core::fmt::Write as _;
        let _ = s.write_str("FOREIGN KEY (");
        for (k, &c) in fk.columns[..fk.n_cols].iter().enumerate() {
            if k > 0 {
                let _ = s.write_str(", ");
            }
            let _ = s.write_str(child.columns()[c as usize].name.as_str());
        }
        let _ = write!(s, ") REFERENCES {}(", fk.parent.as_str());
        for (k, &c) in fk.parent_cols[..fk.n_parent_cols].iter().enumerate() {
            if k > 0 {
                let _ = s.write_str(", ");
            }
            let _ = s.write_str(parent.columns()[c as usize].name.as_str());
        }
        let _ = s.write_str(")");
        let _ = s.write_str(fk_action_suffix(fk.on_delete, "DELETE"));
        let _ = s.write_str(fk_action_suffix(fk.on_update, "UPDATE"));
        return Ok(Some(alloc_rendered(
            &s,
            "foreign key definition is too long",
            arena,
        )?));
    }
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        for (check_index, check) in storage.table_def(slot, txid).checks().iter().enumerate() {
            let check_oid = FIRST_CHECK_OID
                + slot as i32 * crate::storage::MAX_CHECKS as i32
                + check_index as i32;
            if check_oid != oid {
                continue;
            }
            let mut rendered = StackStr::<1024>::new();
            use core::fmt::Write as _;
            let _ = write!(rendered, "CHECK (({}))", check.expression.as_str());
            return Ok(Some(alloc_rendered(
                &rendered,
                "table constraint definition is too long",
                arena,
            )?));
        }
    }
    for slot in 0..storage.domain_count() {
        let domain = storage.domain_for(slot, txid);
        if !domain.visible_to(txid) {
            continue;
        }
        for (check_index, check) in domain.checks().iter().enumerate() {
            let check_oid = FIRST_DOMAIN_CHECK_OID
                + slot as i32 * crate::storage::MAX_DOMAIN_CHECKS as i32
                + check_index as i32;
            if check_oid != oid {
                continue;
            }
            let mut rendered = StackStr::<1024>::new();
            use core::fmt::Write as _;
            let _ = write!(rendered, "CHECK (({}))", check.expression.as_str());
            return Ok(Some(alloc_rendered(
                &rendered,
                "domain constraint definition is too long",
                arena,
            )?));
        }
    }
    let indexes = collect_indexes(storage, txid, arena)?;
    for info in indexes {
        if oid != info.oid + 500_000 || (!info.is_primary && !info.is_unique) {
            continue;
        }
        let table = storage.table_def(info.table_slot, txid);
        let mut rendered = StackStr::<640>::new();
        use core::fmt::Write as _;
        let _ = rendered.write_str(if info.is_primary {
            "PRIMARY KEY ("
        } else {
            "UNIQUE ("
        });
        for (index, &column) in info.columns[..info.n_cols].iter().enumerate() {
            if index > 0 {
                let _ = rendered.write_str(", ");
            }
            let _ = rendered.write_str(table.columns()[column as usize].name.as_str());
        }
        let _ = rendered.write_str(")");
        return Ok(Some(alloc_rendered(
            &rendered,
            "unique constraint definition is too long",
            arena,
        )?));
    }
    Ok(None)
}

/// PostgreSQL omits the clause for the default NO ACTION and spells the others.
fn fk_action_suffix(a: crate::storage::FkAction, event: &str) -> &'static str {
    use crate::storage::FkAction::*;
    match (a, event) {
        (NoAction, _) => "",
        (Restrict, "DELETE") => " ON DELETE RESTRICT",
        (Restrict, _) => " ON UPDATE RESTRICT",
        (Cascade, "DELETE") => " ON DELETE CASCADE",
        (Cascade, _) => " ON UPDATE CASCADE",
        (SetNull, "DELETE") => " ON DELETE SET NULL",
        (SetNull, _) => " ON UPDATE SET NULL",
        (SetDefault, "DELETE") => " ON DELETE SET DEFAULT",
        (SetDefault, _) => " ON UPDATE SET DEFAULT",
    }
}

/// The `btree (col, ...)` index definition psql extracts from `pg_get_indexdef`
/// (it takes everything after `USING`, or the whole string when absent).
pub fn index_def_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    col: usize,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let indices = collect_indexes(storage, txid, arena)?;
    for info in indices {
        if info.oid != oid {
            continue;
        }
        let def = storage.table_def(info.table_slot, txid);
        let col_name = |ci: usize| def.columns()[info.columns[ci] as usize].name.as_str();
        // `col > 0`: just the name of that 1-based indexed column.
        if col > 0 {
            let name = if col <= info.n_cols {
                col_name(col - 1)
            } else {
                return Ok(None);
            };
            let bytes = arena
                .alloc_slice_copy(name.as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
        let mut s = StackStr::<640>::new();
        use core::fmt::Write as _;
        let _ = s.write_str("btree (");
        for k in 0..info.n_cols {
            if k > 0 {
                let _ = s.write_str(", ");
            }
            let _ = s.write_str(col_name(k));
            if info.descending[k] {
                let _ = s.write_str(" DESC");
            }
            if info.nulls_first[k] != info.descending[k] {
                let _ = s.write_str(if info.nulls_first[k] {
                    " NULLS FIRST"
                } else {
                    " NULLS LAST"
                });
            }
        }
        let _ = s.write_str(")");
        return Ok(Some(alloc_rendered(
            &s,
            "index definition is too long",
            arena,
        )?));
    }
    Ok(None)
}

#[derive(Clone, Copy)]
struct SynthDef<'a> {
    name: &'a str,
    columns: &'a [(&'a str, ColType)],
}

fn def_of<'a>(name: &'a str, columns: &'a [(&'a str, ColType)]) -> SynthDef<'a> {
    SynthDef { name, columns }
}

fn materialize_def(specification: SynthDef<'_>) -> TableDef {
    let mut definition = TableDef {
        name: SqlName::parse(specification.name).expect("catalog name fits"),
        columns: [ColumnMeta {
            name: SqlName::parse("").unwrap(),
            ctype: ColType::Bool,
            type_mod: -1,
            not_null: false,
            unique: false,
            primary: false,
            auto_increment: false,
            default: crate::storage::ColumnDefault::NONE,
            is_identity: false,
            identity_always: false,
            auto_increment_step: 1,
            user_type: None,
        }; MAX_COLUMNS],
        n_columns: specification.columns.len(),
        ..TableDef::empty()
    };
    for (index, (name, column_type)) in specification.columns.iter().enumerate() {
        definition.columns[index].name = SqlName::parse(name).expect("catalog column fits");
        definition.columns[index].ctype = *column_type;
    }
    definition
}

/// Allocates `rows` (each a slice already in the arena) as a row slice.
fn finish<'a>(
    specification: SynthDef<'_>,
    rows: &[&'a [Datum<'a>]],
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = arena
        .alloc(materialize_def(specification))
        .map_err(|_| arena_full())?;
    let rows = arena.alloc_slice_copy(rows).map_err(|_| arena_full())?;
    Ok(SynthTable { def, rows: &*rows })
}

fn row<'a>(vals: &[Datum<'a>], arena: &'a Arena) -> Result<&'a [Datum<'a>], SqlError> {
    arena
        .alloc_slice_copy(vals)
        .map(|r| &*r)
        .map_err(|_| arena_full())
}

fn catalog_capacity_exceeded(relation: &str) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "{} exceeds static catalog capacity",
        relation
    )
}

fn text<'a>(s: &str, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    arena
        .alloc_str(s)
        .map(Datum::Text)
        .map_err(|_| arena_full())
}

fn describe_view<'a>(
    storage: &'a Storage,
    txid: u32,
    view: &'a crate::storage::ViewDef,
    arena: &'a Arena,
    out: &mut [super::types::ColDesc<'a>],
) -> Result<usize, SqlError> {
    let user = crate::sql::eval::funcs::system::session_user_owned();
    let path = storage.compute_path(view.creation_path.as_str(), user.as_str(), txid);
    super::query::describe_query_under(view.sql.as_str(), storage, txid, path, arena, out)
}

fn describe_stored_view<'a>(
    storage: &'a Storage,
    txid: u32,
    slot: usize,
    arena: &'a Arena,
    out: &mut [super::types::ColDesc<'a>],
) -> Result<usize, SqlError> {
    let view = storage.view(slot);
    let user = crate::sql::eval::funcs::system::session_user_owned();
    let path = storage.compute_path(view.creation_path.as_str(), user.as_str(), txid);
    super::query::describe_stored_query(
        view.sql.as_str(),
        storage,
        txid,
        path,
        storage.view_dependencies(slot),
        arena,
        out,
    )
}

fn pg_stats<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_stats",
        &[
            ("schemaname", ColType::Text),
            ("tablename", ColType::Text),
            ("attname", ColType::Text),
            ("inherited", ColType::Bool),
            ("null_frac", ColType::Float4),
            ("avg_width", ColType::Int4),
            ("n_distinct", ColType::Float4),
            ("most_common_vals", ColType::Text),
            (
                "most_common_freqs",
                ColType::Array(super::types::ArrElem::Float4),
            ),
            ("histogram_bounds", ColType::Text),
            ("correlation", ColType::Float4),
            ("most_common_elems", ColType::Text),
            (
                "most_common_elem_freqs",
                ColType::Array(super::types::ArrElem::Float4),
            ),
            (
                "elem_count_histogram",
                ColType::Array(super::types::ArrElem::Float4),
            ),
            ("range_length_histogram", ColType::Text),
            ("range_empty_frac", ColType::Float4),
            ("range_bounds_histogram", ColType::Text),
        ],
    );
    let mut rows: [&[Datum]; 512] = [&[]; 512];
    let mut count = 0usize;
    for slot in 0..storage.table_count() {
        let table = storage.table(slot);
        if !table.visible_to(txid) {
            continue;
        }
        let statistics = storage.table_statistics(slot, txid);
        if !statistics.valid {
            continue;
        }
        let table_definition = storage.table_def(slot, txid);
        for (column, metadata) in table_definition.columns().iter().enumerate() {
            let column_statistics = statistics.columns[column];
            if !column_statistics.valid {
                continue;
            }
            if count == rows.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pg_stats exceeds {} rows",
                    rows.len()
                ));
            }
            let distinct = if column_statistics.distinct_fraction_ppm != 0 {
                -(column_statistics.distinct_fraction_ppm as f32 / 1_000_000.0)
            } else {
                column_statistics.distinct_values as f32
            };
            rows[count] = row(
                &[
                    text(table_definition.schema.as_str(), arena)?,
                    text(table_definition.name.as_str(), arena)?,
                    text(metadata.name.as_str(), arena)?,
                    Datum::Bool(false),
                    Datum::Float4(column_statistics.null_fraction_ppm as f32 / 1_000_000.0),
                    Datum::Int4(column_statistics.average_width.min(i32::MAX as u32) as i32),
                    Datum::Float4(distinct),
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(def, &rows[..count], arena)
}

fn publication_oid(slot: usize) -> i32 {
    FIRST_USER_OID + 80_000 + slot as i32
}

fn pg_publication<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_publication",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("pubname", ColType::Text),
            ("pubowner", ColType::Int4),
            ("puballtables", ColType::Bool),
            ("pubinsert", ColType::Bool),
            ("pubupdate", ColType::Bool),
            ("pubdelete", ColType::Bool),
            ("pubtruncate", ColType::Bool),
            ("pubviaroot", ColType::Bool),
            ("pubgencols", ColType::Bpchar),
        ],
    );
    let mut rows: [&[Datum]; 256] = [&[]; 256];
    let mut count = 0;
    for (slot, publication) in storage.publications_with_slots_visible_to(txid) {
        let definition = publication.definition_for(txid);
        rows[count] = row(
            &[
                Datum::Int4(6104),
                Datum::Int4(publication_oid(slot)),
                text(publication.name_for(txid).as_str(), arena)?,
                Datum::Int4(Storage::role_oid(
                    publication.ownership.owner_to(txid) as usize
                )),
                Datum::Bool(definition.all_tables),
                Datum::Bool(definition.publish_insert),
                Datum::Bool(definition.publish_update),
                Datum::Bool(definition.publish_delete),
                Datum::Bool(definition.publish_truncate),
                Datum::Bool(false),
                text("n", arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_publication_namespace<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_publication_namespace",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("pnpubid", ColType::Int4),
            ("pnnspid", ColType::Int4),
        ],
    );
    let mut rows: [&[Datum]; crate::storage::MAX_SCHEMAS * 256] =
        [&[]; crate::storage::MAX_SCHEMAS * 256];
    let mut count = 0;
    for (publication_slot, publication) in storage.publications_with_slots_visible_to(txid) {
        let publication_definition = publication.definition_for(txid);
        for schema_slot in &publication_definition.schemas[..publication_definition.schema_count] {
            if count == rows.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pg_publication_namespace exceeds {} rows",
                    rows.len()
                ));
            }
            let schema = storage.schema_def(*schema_slot as usize);
            rows[count] = row(
                &[
                    Datum::Int4(6105),
                    Datum::Int4(FIRST_USER_OID + 85_000 + count as i32),
                    Datum::Int4(publication_oid(publication_slot)),
                    Datum::Int4(namespace_oid(storage, schema.name.as_str())),
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

fn pg_publication_rel<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_publication_rel",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("prpubid", ColType::Int4),
            ("prrelid", ColType::Int4),
            ("prqual", ColType::Text),
            ("prattrs", ColType::Array(super::types::ArrElem::Int4)),
        ],
    );
    let mut rows: [&[Datum]; 256] = [&[]; 256];
    let mut count = 0;
    for (publication_slot, publication) in storage.publications_with_slots_visible_to(txid) {
        let definition = publication.definition_for(txid);
        if definition.all_tables {
            continue;
        }
        for table_slot in &definition.tables[..definition.table_count] {
            if count == rows.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pg_publication_rel exceeds {} rows",
                    rows.len()
                ));
            }
            rows[count] = row(
                &[
                    Datum::Int4(6106),
                    Datum::Int4(FIRST_USER_OID + 90_000 + count as i32),
                    Datum::Int4(publication_oid(publication_slot)),
                    Datum::Int4(table_oid(storage, *table_slot as usize)),
                    Datum::Null,
                    Datum::Null,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

fn pg_replication_slots<'a>(
    storage: &Storage,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_replication_slots",
        &[
            ("slot_name", ColType::Name),
            ("plugin", ColType::Name),
            ("slot_type", ColType::Text),
            ("datoid", ColType::Int4),
            ("database", ColType::Name),
            ("temporary", ColType::Bool),
            ("active", ColType::Bool),
            ("active_pid", ColType::Int4),
            ("xmin", ColType::Text),
            ("catalog_xmin", ColType::Text),
            ("restart_lsn", ColType::Text),
            ("confirmed_flush_lsn", ColType::Text),
        ],
    );
    let mut rows: [&[Datum]; 256] = [&[]; 256];
    let mut count = 0;
    for (_, slot) in storage.replication_slots_with_slots() {
        if count == rows.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "pg_replication_slots exceeds {} rows",
                rows.len()
            ));
        }
        let restart_lsn = stack_format!(32, "0/{:X}", slot.restart_lsn);
        let confirmed_lsn = stack_format!(32, "0/{:X}", slot.confirmed_flush_lsn);
        rows[count] = row(
            &[
                text(slot.name.as_str(), arena)?,
                text("pgoutput", arena)?,
                text("logical", arena)?,
                Datum::Int4(5),
                text("postgres", arena)?,
                Datum::Bool(false),
                Datum::Bool(slot.active),
                Datum::Null,
                Datum::Null,
                Datum::Null,
                text(restart_lsn.as_str(), arena)?,
                text(confirmed_lsn.as_str(), arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_class<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_class",
        &[
            ("oid", ColType::Int4),
            ("relname", ColType::Text),
            ("relnamespace", ColType::Int4),
            ("relkind", ColType::Bpchar),
            ("relnatts", ColType::Int4),
            ("reltuples", ColType::Float8),
            ("relpages", ColType::Int4),
            ("relam", ColType::Int4),
            ("relowner", ColType::Int4),
            ("relchecks", ColType::Int2),
            ("relhasindex", ColType::Bool),
            ("relhasrules", ColType::Bool),
            ("relhastriggers", ColType::Bool),
            ("relrowsecurity", ColType::Bool),
            ("relforcerowsecurity", ColType::Bool),
            ("relispartition", ColType::Bool),
            ("reltablespace", ColType::Int4),
            ("reloftype", ColType::Int4),
            ("reltoastrelid", ColType::Int4),
            ("relpersistence", ColType::Bpchar),
            ("relreplident", ColType::Bpchar),
            ("tableoid", ColType::Int4),
            ("reltype", ColType::Int4),
            ("relacl", ColType::Array(super::types::ArrElem::Text)),
            ("relallvisible", ColType::Int4),
            ("relallfrozen", ColType::Int4),
            ("relfrozenxid", ColType::Int4),
            ("relminmxid", ColType::Int4),
            ("reloptions", ColType::Array(super::types::ArrElem::Text)),
            ("relispopulated", ColType::Bool),
        ],
    );
    let indexes = collect_indexes(storage, txid, arena)?;
    let foreign_keys = collect_fkeys(storage, txid, arena)?;
    let mut out: [&[Datum]; 512] = [&[]; 512];
    let mut n = 0;
    for slot in 0..storage.table_count() {
        let table = storage.table(slot);
        if !table.visible_to(txid) {
            continue;
        }
        let table_def = storage.table_def(slot, txid);
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_class"));
        }
        let toid = table_oid(storage, slot);
        let has_index = indexes.iter().any(|i| i.table_oid == toid);
        let has_triggers = !table_def.fkeys().is_empty()
            || foreign_keys
                .iter()
                .any(|foreign_key| foreign_key.confrelid == toid);
        let n_checks = table_def.n_checks as i32;
        let statistics = storage.table_statistics(slot, txid);
        let reltuples = if statistics.valid {
            statistics.rows as f64
        } else {
            -1.0
        };
        let relpages = if statistics.valid {
            statistics
                .rows
                .saturating_mul(u64::from(statistics.average_row_width))
                .div_ceil(8_192)
                .min(i32::MAX as u64) as i32
        } else {
            0
        };
        // A table that has a matching matview catalog entry is a materialized
        // view (relkind 'm'), not an ordinary table ('r').
        let relkind = if storage
            .find_matview(table_def.schema.as_str(), table_def.name.as_str(), txid)
            .is_some()
        {
            "m"
        } else {
            "r"
        };
        let relation_object = storage
            .matview_slot(table_def.schema.as_str(), table_def.name.as_str(), txid)
            .map_or(
                crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Table,
                    slot: slot as u16,
                },
                |matview| crate::storage::AccessObject {
                    class: crate::storage::AccessClass::MaterializedView,
                    slot: matview as u16,
                },
            );
        let relation_owner = Storage::role_oid(storage.object_owner(relation_object, txid));
        out[n] = row(
            &[
                Datum::Int4(toid),
                text(table_def.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, table_def.schema.as_str())),
                text(relkind, arena)?, // relkind: ordinary table 'r' / matview 'm'
                Datum::Int4(table_def.n_columns as i32),
                Datum::Float8(reltuples),
                Datum::Int4(relpages),
                Datum::Int4(0), // relam
                Datum::Int4(relation_owner),
                Datum::Int4(n_checks), // relchecks
                Datum::Bool(has_index),
                Datum::Bool(false),        // relhasrules
                Datum::Bool(has_triggers), // FK enforcement is trigger-backed in PostgreSQL
                Datum::Bool(false),        // relrowsecurity
                Datum::Bool(false),        // relforcerowsecurity
                Datum::Bool(false),        // relispartition
                Datum::Int4(0),            // reltablespace
                Datum::Int4(0),            // reloftype
                Datum::Int4(0),            // reltoastrelid
                text("p", arena)?,         // relpersistence: permanent
                text("d", arena)?,         // relreplident: default
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(FIRST_TABLE_COMPOSITE_TYPE_OID + slot as i32),
                acl(storage, relation_object, txid, arena)?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Null,
                Datum::Bool(true),
            ],
            arena,
        )?;
        n += 1;
    }
    // Each index is itself a relation (relkind 'i'), so psql's `\d` join
    // (pg_index i JOIN pg_class c2 ON i.indexrelid = c2.oid) finds its name.
    for info in indexes {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_class"));
        }
        out[n] = row(
            &[
                Datum::Int4(info.oid),
                text(info.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(
                    storage,
                    storage.table_def(info.table_slot, txid).schema.as_str(),
                )),
                text("i", arena)?, // relkind: index
                Datum::Int4(info.n_cols as i32),
                Datum::Float8(0.0),
                Datum::Int4(0),   // relpages
                Datum::Int4(403), // relam: btree
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Table,
                    info.table_slot,
                    txid,
                )),
                Datum::Int4(0), // relchecks
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                text("p", arena)?,
                text("d", arena)?,
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(0),
                Datum::Null,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Null,
                Datum::Bool(true),
            ],
            arena,
        )?;
        n += 1;
    }
    // Sequences are relations of kind 'S', each with its own OID range so
    // psql's `\d`/`\dm` and pg_get_serial_sequence-style joins resolve.
    for slot in 0..storage.sequence_count() {
        let seq = storage.sequence_for(slot, txid);
        if !seq.visible_to(txid) {
            continue;
        }
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_class"));
        }
        out[n] = row(
            &[
                Datum::Int4(sequence_oid(slot)),
                text(seq.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, seq.schema.as_str())),
                text("S", arena)?, // relkind: sequence
                Datum::Int4(1),    // a sequence has one row of state
                Datum::Float8(1.0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Sequence,
                    slot,
                    txid,
                )),
                Datum::Int4(0),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                text("p", arena)?,
                text("n", arena)?, // relreplident: nothing (sequences)
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(0),
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Sequence,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Null,
                Datum::Bool(true),
            ],
            arena,
        )?;
        n += 1;
    }
    // Plain views are pg_class relations too (relkind 'v'). Their column count
    // is derived under the creator's captured search path, the same binding
    // rule used when the view executes.
    for (slot, view) in storage.views_visible_to(txid) {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_class"));
        }
        let mut columns = [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
        let n_columns = describe_view(storage, txid, view, arena, &mut columns)?;
        out[n] = row(
            &[
                Datum::Int4(view_oid(slot)),
                text(view.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, view.schema.as_str())),
                text("v", arena)?,
                Datum::Int4(n_columns as i32),
                Datum::Float8(0.0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::View,
                    slot,
                    txid,
                )),
                Datum::Int4(0),
                Datum::Bool(false),
                Datum::Bool(true), // a view is represented by a rewrite rule
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                text("p", arena)?,
                text("n", arena)?,
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(FIRST_VIEW_COMPOSITE_TYPE_OID + slot as i32),
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::View,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Null,
                Datum::Bool(true),
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_constraint<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_constraint",
        &[
            ("oid", ColType::Int4),
            ("conname", ColType::Text),
            ("conrelid", ColType::Int4),
            ("contypid", ColType::Int4),
            ("contype", ColType::Bpchar),
            ("conparentid", ColType::Int4),
            ("conindid", ColType::Int4),
            ("confrelid", ColType::Int4),
            ("condeferrable", ColType::Bool),
            ("condeferred", ColType::Bool),
            ("convalidated", ColType::Bool),
            ("conperiod", ColType::Bool),
            ("confupdtype", ColType::Bpchar),
            ("confdeltype", ColType::Bpchar),
            ("conkey", ColType::Array(super::types::ArrElem::Int4)),
            ("confkey", ColType::Array(super::types::ArrElem::Int4)),
            ("tableoid", ColType::Int4),
            ("connamespace", ColType::Int4),
            ("confmatchtype", ColType::Bpchar),
            ("conislocal", ColType::Bool),
            ("coninhcount", ColType::Int4),
            ("connoinherit", ColType::Bool),
            ("conpfeqop", ColType::Array(super::types::ArrElem::Int4)),
            ("conppeqop", ColType::Array(super::types::ArrElem::Int4)),
            ("conffeqop", ColType::Array(super::types::ArrElem::Int4)),
            (
                "confdelsetcols",
                ColType::Array(super::types::ArrElem::Int4),
            ),
            ("conexclop", ColType::Array(super::types::ArrElem::Int4)),
            ("conbin", ColType::Text),
        ],
    );
    let indexes = collect_indexes(storage, txid, arena)?;
    let mut out: [&[Datum]; 512] = [&[]; 512];
    let mut n = 0;
    // A PRIMARY KEY or UNIQUE constraint has a backing index; its `conindid`
    // links to that index so psql's `\d` labels a UNIQUE index as a constraint.
    for info in indexes {
        let contype = if info.is_primary {
            "p"
        } else if info.is_unique {
            "u"
        } else {
            continue;
        };
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_constraint"));
        }
        out[n] = row(
            &[
                Datum::Int4(info.oid + 500_000), // constraint oid, distinct from the index's
                text(info.name.as_str(), arena)?,
                Datum::Int4(info.table_oid),
                Datum::Int4(0),
                text(contype, arena)?,
                Datum::Int4(0),        // conparentid
                Datum::Int4(info.oid), // conindid -> the backing index
                Datum::Int4(0),        // confrelid
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(true),  // convalidated
                Datum::Bool(false), // conperiod
                text(" ", arena)?,  // confupdtype (n/a for non-FK)
                text(" ", arena)?,  // confdeltype
                attnum_array(&info.columns[..info.n_cols], arena)?,
                empty_int_array(arena)?,
                Datum::Int4(2606),
                Datum::Int4(namespace_oid(
                    storage,
                    storage.table_def(info.table_slot, txid).schema.as_str(),
                )),
                text(" ", arena)?,
                Datum::Bool(true),
                Datum::Int4(0),
                Datum::Bool(false),
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // Foreign-key constraints (contype 'f'): conrelid on the child, confrelid on
    // the referenced parent, so psql's "Foreign-key constraints" (child) and
    // "Referenced by" (parent) sections both resolve.
    let fks = collect_fkeys(storage, txid, arena)?;
    for info in fks {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_constraint"));
        }
        let fk = &storage.table_def(info.child_slot, txid).fkeys()[info.fk_index];
        // conindid points at the parent's unique/PK index backing the referenced
        // columns, which JDBC joins to for foreign-key metadata.
        let conindid = indexes
            .iter()
            .find(|ix| {
                ix.table_oid == info.confrelid
                    && ix.n_cols == fk.n_parent_cols
                    && ix.columns[..ix.n_cols] == fk.parent_cols[..fk.n_parent_cols]
            })
            .map_or(0, |ix| ix.oid);
        out[n] = row(
            &[
                Datum::Int4(info.oid),
                text(info.name.as_str(), arena)?,
                Datum::Int4(info.conrelid),
                Datum::Int4(0),
                text("f", arena)?,
                Datum::Int4(0), // conparentid
                Datum::Int4(conindid),
                Datum::Int4(info.confrelid),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(true),
                Datum::Bool(false),
                text(fk_action_char(fk.on_update), arena)?,
                text(fk_action_char(fk.on_delete), arena)?,
                attnum_array(&fk.columns[..fk.n_cols], arena)?,
                attnum_array(&fk.parent_cols[..fk.n_parent_cols], arena)?,
                Datum::Int4(2606),
                Datum::Int4(namespace_oid(
                    storage,
                    storage.table_def(info.child_slot, txid).schema.as_str(),
                )),
                text("s", arena)?,
                Datum::Bool(true),
                Datum::Int4(0),
                Datum::Bool(false),
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // CHECK constraints are catalog objects too. Their source predicate is
    // preserved by the table definition and reconstructed by
    // pg_get_constraintdef, which is what psql's "Check constraints" section
    // reads.
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        for (check_index, check) in storage.table_def(slot, txid).checks().iter().enumerate() {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_constraint"));
            }
            out[n] = row(
                &[
                    Datum::Int4(
                        FIRST_CHECK_OID
                            + slot as i32 * crate::storage::MAX_CHECKS as i32
                            + check_index as i32,
                    ),
                    text(check.name.as_str(), arena)?,
                    Datum::Int4(table_oid(storage, slot)),
                    Datum::Int4(0),
                    text("c", arena)?,
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Bool(false),
                    Datum::Bool(false),
                    Datum::Bool(true),
                    Datum::Bool(false),
                    text(" ", arena)?,
                    text(" ", arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    Datum::Int4(2606),
                    Datum::Int4(namespace_oid(
                        storage,
                        storage.table_def(slot, txid).schema.as_str(),
                    )),
                    text(" ", arena)?,
                    Datum::Bool(true),
                    Datum::Int4(0),
                    Datum::Bool(false),
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    // PostgreSQL 18 represents NOT NULL constraints in pg_constraint as well
    // as pg_attribute. pg_dump uses these rows to preserve the constraint
    // before adding an identity property in its dependency-ordered output.
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for (column_index, column) in table.columns().iter().enumerate() {
            if !column.not_null {
                continue;
            }
            if n == out.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pg_constraint result exceeds static capacity"
                ));
            }
            let constraint_name = not_null_constraint_name(table, column);
            out[n] = row(
                &[
                    Datum::Int4(
                        FIRST_NOT_NULL_OID + slot as i32 * MAX_COLUMNS as i32 + column_index as i32,
                    ),
                    text(constraint_name.as_str(), arena)?,
                    Datum::Int4(table_oid(storage, slot)),
                    Datum::Int4(0),
                    text("n", arena)?,
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Bool(false),
                    Datum::Bool(false),
                    Datum::Bool(true),
                    Datum::Bool(false),
                    text(" ", arena)?,
                    text(" ", arena)?,
                    attnum_array(&[column_index as u16], arena)?,
                    empty_int_array(arena)?,
                    Datum::Int4(2606),
                    Datum::Int4(namespace_oid(storage, table.schema.as_str())),
                    text(" ", arena)?,
                    Datum::Bool(true),
                    Datum::Int4(0),
                    Datum::Bool(false),
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    // Domain CHECK constraints are attached to `contypid`, not a table.
    for slot in 0..storage.domain_count() {
        let domain = storage.domain_for(slot, txid);
        if !domain.visible_to(txid) {
            continue;
        }
        for (check_index, check) in domain.checks().iter().enumerate() {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_constraint"));
            }
            out[n] = row(
                &[
                    Datum::Int4(
                        FIRST_DOMAIN_CHECK_OID
                            + slot as i32 * crate::storage::MAX_DOMAIN_CHECKS as i32
                            + check_index as i32,
                    ),
                    text(check.name.as_str(), arena)?,
                    Datum::Int4(0),
                    Datum::Int4(domain_oid(slot)),
                    text("c", arena)?,
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Bool(false),
                    Datum::Bool(false),
                    Datum::Bool(true),
                    Datum::Bool(false),
                    text(" ", arena)?,
                    text(" ", arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    Datum::Int4(2606),
                    Datum::Int4(namespace_oid(storage, domain.schema.as_str())),
                    text(" ", arena)?,
                    Datum::Bool(true),
                    Datum::Int4(0),
                    Datum::Bool(false),
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    finish(def, &out[..n], arena)
}

fn pg_rewrite<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_rewrite",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("rulename", ColType::Text),
            ("ev_class", ColType::Int4),
            ("ev_type", ColType::Bpchar),
            ("ev_enabled", ColType::Bpchar),
            ("is_instead", ColType::Bool),
        ],
    );
    let count = storage.views_visible_to(txid).count();
    let out = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (slot, _)) in storage.views_visible_to(txid).enumerate() {
        out[index] = row(
            &[
                Datum::Int4(2618),
                Datum::Int4(view_rewrite_oid(slot)),
                text("_RETURN", arena)?,
                Datum::Int4(view_oid(slot)),
                text("1", arena)?,
                text("O", arena)?,
                Datum::Bool(true),
            ],
            arena,
        )?;
    }
    finish(def, out, arena)
}

fn pg_depend<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_depend",
        &[
            ("classid", ColType::Int4),
            ("objid", ColType::Int4),
            ("objsubid", ColType::Int4),
            ("refclassid", ColType::Int4),
            ("refobjid", ColType::Int4),
            ("refobjsubid", ColType::Int4),
            ("deptype", ColType::Bpchar),
        ],
    );
    let mut out: [&[Datum]; 4096] = [&[]; 4096];
    let mut count = 0usize;
    let mut push = |class: i32,
                    object: i32,
                    referenced_class: i32,
                    referenced_object: i32,
                    referenced_subobject: i32,
                    dependency_type: &str|
     -> Result<(), SqlError> {
        if count == out.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "catalog dependency result exceeds static capacity"
            ));
        }
        out[count] = row(
            &[
                Datum::Int4(class),
                Datum::Int4(object),
                Datum::Int4(0),
                Datum::Int4(referenced_class),
                Datum::Int4(referenced_object),
                Datum::Int4(referenced_subobject),
                text(dependency_type, arena)?,
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };

    // Sequence ownership is how pg_dump distinguishes serial/identity
    // generators from independent sequences and orders them with the owning
    // column.
    for sequence_slot in 0..storage.sequence_count() {
        let sequence = storage.sequence_for(sequence_slot, txid);
        if !sequence.visible_to(txid) {
            continue;
        }
        let Some(owner) = sequence.owner else {
            continue;
        };
        let Some(table_slot) =
            storage.find_visible(owner.table_schema.as_str(), owner.table.as_str(), txid)
        else {
            continue;
        };
        let table = storage.table_def(table_slot, txid);
        let Some(column) = table.column_index(owner.column.as_str()) else {
            continue;
        };
        push(
            PG_CLASS_OID,
            sequence_oid(sequence_slot),
            PG_CLASS_OID,
            table_oid(storage, table_slot),
            column as i32 + 1,
            if table.columns()[column].is_identity {
                "i"
            } else {
                "a"
            },
        )?;
    }

    let referenced_oid = |dependency: &crate::storage::StoredQueryDependency| match dependency.class
    {
        crate::storage::DependencyClass::Table => {
            Some((PG_CLASS_OID, table_oid(storage, dependency.slot as usize)))
        }
        crate::storage::DependencyClass::View => {
            Some((PG_CLASS_OID, view_oid(dependency.slot as usize)))
        }
        crate::storage::DependencyClass::Sequence => {
            Some((PG_CLASS_OID, sequence_oid(dependency.slot as usize)))
        }
        crate::storage::DependencyClass::Domain => {
            Some((PG_TYPE_OID, domain_oid(dependency.slot as usize)))
        }
        crate::storage::DependencyClass::Enum => Some((
            PG_TYPE_OID,
            crate::sql::types::oid::enum_oid(dependency.slot),
        )),
    };
    for (view_slot, _) in storage.views_visible_to(txid) {
        push(
            2618,
            view_rewrite_oid(view_slot),
            PG_CLASS_OID,
            view_oid(view_slot),
            0,
            "i",
        )?;
        for dependency in storage.view_dependencies(view_slot).entries() {
            let Some((referenced_class, referenced_object)) = referenced_oid(dependency) else {
                continue;
            };
            if dependency.referenced_columns == 0 {
                push(
                    2618,
                    view_rewrite_oid(view_slot),
                    referenced_class,
                    referenced_object,
                    0,
                    "n",
                )?;
            } else {
                for column in 0..u64::BITS as usize {
                    if dependency.referenced_columns & (1u64 << column) != 0 {
                        push(
                            2618,
                            view_rewrite_oid(view_slot),
                            referenced_class,
                            referenced_object,
                            column as i32 + 1,
                            "n",
                        )?;
                    }
                }
            }
        }
    }
    for (materialized_slot, materialized_view) in storage.matviews_visible_to(txid) {
        let Some(table_slot) = storage.find_visible(
            materialized_view.schema.as_str(),
            materialized_view.name.as_str(),
            txid,
        ) else {
            continue;
        };
        for dependency in storage.matview_dependencies(materialized_slot).entries() {
            let Some((referenced_class, referenced_object)) = referenced_oid(dependency) else {
                continue;
            };
            push(
                PG_CLASS_OID,
                table_oid(storage, table_slot),
                referenced_class,
                referenced_object,
                0,
                "n",
            )?;
        }
    }
    finish(def, &out[..count], arena)
}

/// PostgreSQL's `confupdtype`/`confdeltype` code for a referential action.
fn fk_action_char(a: crate::storage::FkAction) -> &'static str {
    use crate::storage::FkAction::*;
    match a {
        NoAction => "a",
        Restrict => "r",
        Cascade => "c",
        SetNull => "n",
        SetDefault => "d",
    }
}

/// A `Datum::Array` of 1-based attribute numbers (column index + 1), the form
/// `conkey`/`confkey`/`indkey`-as-array take in `pg_constraint`.
fn attnum_array<'a>(columns: &[u16], arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let mut vals = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    for (i, &c) in columns.iter().enumerate() {
        vals[i] = Datum::Int4(i32::from(c) + 1);
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Int4,
        raw: super::array::build(&vals[..columns.len()], arena)?,
    })
}

fn empty_int_array<'a>(arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    Ok(Datum::Array {
        element: super::types::ArrElem::Int4,
        raw: super::array::build(&[], arena)?,
    })
}

fn pg_index<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_index",
        &[
            ("indexrelid", ColType::Int4),
            ("indrelid", ColType::Int4),
            ("indisprimary", ColType::Bool),
            ("indisunique", ColType::Bool),
            ("indisclustered", ColType::Bool),
            ("indisvalid", ColType::Bool),
            ("indimmediate", ColType::Bool),
            ("indisreplident", ColType::Bool),
            ("indnullsnotdistinct", ColType::Bool),
            ("indnatts", ColType::Int4),
            ("indnkeyatts", ColType::Int4),
            ("indkey", ColType::Int2Vector),
            ("indoption", ColType::Array(super::types::ArrElem::Int4)),
            ("indpred", ColType::Text),
            ("indisready", ColType::Bool),
            ("indexprs", ColType::Text),
            ("indcollation", ColType::Array(super::types::ArrElem::Int4)),
            ("indclass", ColType::Array(super::types::ArrElem::Int4)),
        ],
    );
    let indexes = collect_indexes(storage, txid, arena)?;
    let out = arena
        .alloc_slice_with(indexes.len(), |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut n = 0;
    for info in indexes {
        // indkey is the 1-based attribute numbers as an int2vector-like array;
        // indoption is one flag per column (0 = default ascending).
        let zeros = [0u16; crate::storage::MAX_INDEX_COLS];
        out[n] = row(
            &[
                Datum::Int4(info.oid),
                Datum::Int4(info.table_oid),
                Datum::Bool(info.is_primary),
                Datum::Bool(info.is_unique),
                Datum::Bool(false), // indisclustered
                Datum::Bool(true),  // indisvalid
                Datum::Bool(true),  // constraints are checked immediately
                Datum::Bool(false), // indisreplident
                Datum::Bool(false), // NULL values are distinct by default
                Datum::Int4(info.n_cols as i32),
                Datum::Int4(info.n_cols as i32),
                int2vector(&info.columns[..info.n_cols], arena)?,
                option_array(&zeros[..info.n_cols], arena)?,
                Datum::Null, // partial indexes are not yet accepted by the DDL grammar
                Datum::Bool(true),
                Datum::Null,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

/// An index-option array (one 0-flag per column) for `pg_index.indoption`.
fn option_array<'a>(columns: &[u16], arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let mut vals = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    for (i, _) in columns.iter().enumerate() {
        vals[i] = Datum::Int4(0);
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Int4,
        raw: super::array::build(&vals[..columns.len()], arena)?,
    })
}

fn int2vector<'a>(columns: &[u16], arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let raw = arena
        .alloc_slice_with(columns.len() * 2, |_| 0u8)
        .map_err(|_| arena_full())?;
    for (index, column) in columns.iter().enumerate() {
        raw[index * 2..index * 2 + 2].copy_from_slice(&(*column as i16 + 1).to_le_bytes());
    }
    Ok(Datum::Int2Vector(raw))
}

fn catalog_column_type_oid(
    storage: &Storage,
    column: &ColumnMeta,
    txid: u32,
) -> Result<i32, SqlError> {
    Ok(storage.declared_column_type(column, txid)?.catalog_oid())
}

fn pg_attribute<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_attribute",
        &[
            ("attrelid", ColType::Int4),
            ("attname", ColType::Text),
            ("atttypid", ColType::Int4),
            ("attnum", ColType::Int4),
            ("attnotnull", ColType::Bool),
            ("attlen", ColType::Int4),
            ("atttypmod", ColType::Int4),
            ("atthasdef", ColType::Bool),
            ("attcollation", ColType::Int4),
            ("attidentity", ColType::Bpchar),
            ("attgenerated", ColType::Bpchar),
            ("attstorage", ColType::Bpchar),
            ("attcompression", ColType::Bpchar),
            ("attstattarget", ColType::Int4),
            ("attisdropped", ColType::Bool),
            ("attnum_ord", ColType::Int4),
            ("attalign", ColType::Bpchar),
            ("attislocal", ColType::Bool),
            ("attoptions", ColType::Array(super::types::ArrElem::Text)),
            ("attfdwoptions", ColType::Array(super::types::ArrElem::Text)),
            ("atthasmissing", ColType::Bool),
            ("attmissingval", ColType::Text),
            ("attacl", ColType::Array(super::types::ArrElem::Text)),
        ],
    );
    let mut out: [&[Datum]; 1024] = [&[]; 1024];
    let mut n = 0;
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        for (i, c) in storage.table_def(slot, txid).columns().iter().enumerate() {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_attribute"));
            }
            out[n] = row(
                &[
                    Datum::Int4(table_oid(storage, slot)),
                    text(c.name.as_str(), arena)?,
                    Datum::Int4(catalog_column_type_oid(storage, c, txid)?),
                    Datum::Int4(i as i32 + 1),
                    Datum::Bool(c.not_null),
                    Datum::Int4(i32::from(c.ctype.typlen())),
                    Datum::Int4(c.type_mod),
                    Datum::Bool(!matches!(c.default, crate::storage::ColumnDefault::None)),
                    Datum::Int4(0), // attcollation: default (0)
                    text(
                        if c.is_identity && c.identity_always {
                            "a"
                        } else if c.is_identity {
                            "d"
                        } else {
                            ""
                        },
                        arena,
                    )?, // attidentity: always 'a' / by default 'd'
                    text(if c.default.is_generated() { "s" } else { "" }, arena)?, // attgenerated
                    // Fixed-width values are plain; variable-width values use
                    // PostgreSQL's ordinary extended storage policy.
                    text(if c.ctype.typlen() < 0 { "x" } else { "p" }, arena)?,
                    text("", arena)?,   // attcompression: type default
                    Datum::Int4(-1),    // attstattarget: use server default
                    Datum::Bool(false), // attisdropped
                    Datum::Int4(i as i32 + 1),
                    text("i", arena)?,
                    Datum::Bool(true),
                    Datum::Null,
                    Datum::Null,
                    Datum::Bool(false),
                    Datum::Null,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    let indexes = collect_indexes(storage, txid, arena)?;
    for info in indexes {
        let table = storage.table_def(info.table_slot, txid);
        for (attribute, &column_index) in info.columns[..info.n_cols].iter().enumerate() {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_attribute"));
            }
            let column = &table.columns()[column_index as usize];
            out[n] = row(
                &[
                    Datum::Int4(info.oid),
                    text(column.name.as_str(), arena)?,
                    Datum::Int4(catalog_column_type_oid(storage, column, txid)?),
                    Datum::Int4(attribute as i32 + 1),
                    Datum::Bool(false),
                    Datum::Int4(i32::from(column.ctype.typlen())),
                    Datum::Int4(column.type_mod),
                    Datum::Bool(false),
                    Datum::Int4(0),
                    text("", arena)?,
                    text("", arena)?,
                    text(if column.ctype.typlen() < 0 { "x" } else { "p" }, arena)?,
                    text("", arena)?,
                    Datum::Int4(-1),
                    Datum::Bool(false),
                    Datum::Int4(attribute as i32 + 1),
                    text("i", arena)?,
                    Datum::Bool(true),
                    Datum::Null,
                    Datum::Null,
                    Datum::Bool(false),
                    Datum::Null,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    for (slot, view) in storage.views_visible_to(txid) {
        let mut columns = [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
        let count = describe_view(storage, txid, view, arena, &mut columns)?;
        for (attribute, column) in columns[..count].iter().enumerate() {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_attribute"));
            }
            let (ctype, _) = view_column_catalog_type(storage, txid, column.type_oid)?;
            out[n] = row(
                &[
                    Datum::Int4(view_oid(slot)),
                    text(column.name, arena)?,
                    Datum::Int4(column.type_oid),
                    Datum::Int4(attribute as i32 + 1),
                    Datum::Bool(false),
                    Datum::Int4(i32::from(column.typlen)),
                    Datum::Int4(column.type_mod),
                    Datum::Bool(false),
                    Datum::Int4(0),
                    text("", arena)?,
                    text("", arena)?,
                    text(if ctype.typlen() < 0 { "x" } else { "p" }, arena)?,
                    text("", arena)?,
                    Datum::Int4(-1),
                    Datum::Bool(false),
                    Datum::Int4(attribute as i32 + 1),
                    text("i", arena)?,
                    Datum::Bool(true),
                    Datum::Null,
                    Datum::Null,
                    Datum::Bool(false),
                    Datum::Null,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    finish(def, &out[..n], arena)
}

fn pg_attrdef<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_attrdef",
        &[
            ("oid", ColType::Int4),
            ("adrelid", ColType::Int4),
            ("adnum", ColType::Int4),
            ("adbin", ColType::Text),
            ("tableoid", ColType::Int4),
        ],
    );
    // A row per column carrying a DEFAULT — the raw source text in `adbin`.
    // This is the engine's source text, not PostgreSQL's serialized node tree;
    // pg_get_expr exposes it through the same catalog contract.
    let mut out: [&[Datum]; 512] = [&[]; 512];
    let mut n = 0;
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        let relid = table_oid(storage, slot);
        for (ci, c) in table.columns().iter().enumerate() {
            let Some(text_expr) = c.default.expression() else {
                continue;
            };
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_attrdef"));
            }
            out[n] = row(
                &[
                    Datum::Int4(relid * 100 + ci as i32 + 1), // synthetic adbin oid
                    Datum::Int4(relid),
                    Datum::Int4(ci as i32 + 1),
                    text(text_expr.as_str(), arena)?,
                    Datum::Int4(2604),
                ],
                arena,
            )?;
            n += 1;
        }
    }
    finish(def, &out[..n], arena)
}

fn pg_proc<'a>(storage: &Storage, txid: u32, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_proc",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("proname", ColType::Name),
            ("pronamespace", ColType::Oid),
            ("pronargs", ColType::Int4),
            ("prorettype", ColType::Oid),
            ("prokind", ColType::Bpchar),
            ("proargtypes", ColType::Text),
            ("provolatile", ColType::Bpchar),
            ("proparallel", ColType::Bpchar),
            ("proowner", ColType::Oid),
            ("prosecdef", ColType::Bool),
            ("proacl", ColType::Array(super::types::ArrElem::Text)),
            ("prolang", ColType::Oid),
            ("prosrc", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = 512;
    let mut rows: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    for (index, routine) in INTRINSIC_ROUTINES.iter().enumerate() {
        rows[index] = row(
            &[
                Datum::Int4(1255),
                Datum::Int4(routine.oid),
                text(routine.name, arena)?,
                Datum::Int4(PG_CATALOG_NS_OID),
                Datum::Int4(routine.argument_count),
                Datum::Int4(routine.result_oid),
                Datum::Bpchar("f"),
                text(routine.argument_types, arena)?,
                Datum::Bpchar(routine.volatility),
                Datum::Bpchar("s"),
                Datum::Int4(10),
                Datum::Bool(false),
                Datum::Null,
                Datum::Int4(12),
                text(
                    if routine.oid == 89 {
                        "pgsql_version"
                    } else {
                        routine.name
                    },
                    arena,
                )?,
            ],
            arena,
        )?;
    }
    let mut count = INTRINSIC_ROUTINES.len();
    for slot in 0..storage.routine_count() {
        let routine = storage.routine(slot);
        if !routine.visible_to(txid) {
            continue;
        }
        if count == rows.len() {
            return Err(catalog_capacity_exceeded("pg_proc"));
        }
        let mut argument_types = crate::util::StackStr::<128>::new();
        for (index, argument) in routine.arguments().iter().enumerate() {
            if index > 0 {
                let _ = core::fmt::Write::write_str(&mut argument_types, " ");
            }
            let _ = core::fmt::Write::write_fmt(
                &mut argument_types,
                format_args!("{}", argument.ctype.oid()),
            );
        }
        rows[count] = row(
            &[
                Datum::Int4(1255),
                Datum::Int4(crate::storage::routine_oid(routine)),
                text(routine.name_for(txid).as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, routine.schema_for(txid).as_str())),
                Datum::Int4(routine.argument_count as i32),
                Datum::Int4(
                    routine
                        .kind
                        .function_result()
                        .map(ColType::oid)
                        .unwrap_or(2278),
                ),
                Datum::Bpchar(routine.kind.catalog_kind()),
                text(argument_types.as_str(), arena)?,
                Datum::Bpchar("v"),
                Datum::Bpchar("u"),
                Datum::Int4(Storage::role_oid(routine.ownership.owner_to(txid) as usize)),
                Datum::Bool(false),
                acl(storage, Storage::routine_access_object(slot), txid, arena)?,
                Datum::Int4(14),
                text(routine.body.as_str(), arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_collation<'a>(arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_collation",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("collname", ColType::Text),
            ("collnamespace", ColType::Int4),
            ("collowner", ColType::Int4),
            ("collcollate", ColType::Text),
            ("collctype", ColType::Text),
            ("colliculocale", ColType::Text),
            ("collprovider", ColType::Bpchar),
            ("collisdeterministic", ColType::Bool),
            ("collencoding", ColType::Int4),
        ],
    );
    let rows = [
        (100, "default", "", "", "d", -1),
        (950, "C", "C", "C", "c", -1),
        (951, "POSIX", "POSIX", "POSIX", "c", -1),
        (12_340, "ucs_basic", "", "", "b", 6),
    ];
    let mut output: [&[Datum]; 4] = [&[]; 4];
    for (index, (oid, name, collate, ctype, provider, encoding)) in rows.iter().enumerate() {
        output[index] = row(
            &[
                Datum::Int4(*oid),
                Datum::Int4(*oid),
                text(name, arena)?,
                Datum::Int4(PG_CATALOG_NS_OID),
                Datum::Int4(10),
                text(collate, arena)?,
                text(ctype, arena)?,
                Datum::Null,
                Datum::Bpchar(provider),
                Datum::Bool(true),
                Datum::Int4(*encoding),
            ],
            arena,
        )?;
    }
    finish(definition, &output, arena)
}

fn pg_enum<'a>(storage: &Storage, txid: u32, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_enum",
        &[
            ("oid", ColType::Int4),
            ("enumtypid", ColType::Int4),
            ("enumsortorder", ColType::Float8),
            ("enumlabel", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = crate::storage::MAX_ENUMS * crate::storage::MAX_ENUM_LABELS;
    let mut out: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut n = 0;
    for slot in 0..storage.enum_count() {
        let e = storage.enum_for(slot, txid);
        if !e.visible_to(txid) {
            continue;
        }
        let typid = crate::sql::types::oid::enum_oid(slot as u16);
        for (i, m) in e.members().iter().enumerate() {
            out[n] = row(
                &[
                    // A stable, unique synthetic OID per member.
                    Datum::Int4(typid * 1000 + i as i32),
                    Datum::Int4(typid),
                    Datum::Float8(m.sort),
                    text(
                        arena
                            .alloc_str(m.label.as_str())
                            .map_err(|_| arena_full())?,
                        arena,
                    )?,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    finish(def, &out[..n], arena)
}

fn pg_type<'a>(storage: &Storage, txid: u32, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_type",
        &[
            ("oid", ColType::Int4),
            ("typname", ColType::Name),
            ("typlen", ColType::Int4),
            ("typcollation", ColType::Int4),
            ("typnamespace", ColType::Int4),
            ("typtype", ColType::Bpchar), // 'b' = base type
            ("typcategory", ColType::Bpchar),
            ("typbasetype", ColType::Int4), // 0 unless a domain
            ("typelem", ColType::Int4),     // element type of an array, else 0
            ("typarray", ColType::Int4),    // the array type over this type, else 0
            ("typrelid", ColType::Int4),    // 0 unless a composite type
            ("typtypmod", ColType::Int4),
            ("typnotnull", ColType::Bool),
            ("typdefault", ColType::Text),
            ("typinput", ColType::Text),
            ("typoutput", ColType::Text),
            ("typacl", ColType::Array(super::types::ArrElem::Text)),
            ("tableoid", ColType::Int4),
            ("typowner", ColType::Int4),
            ("typisdefined", ColType::Bool),
            ("typstorage", ColType::Bpchar),
        ],
    );
    let types = [
        ColType::Bool,
        ColType::Int4,
        ColType::Int8,
        ColType::Float8,
        ColType::Text,
        ColType::Date,
        ColType::Timestamp,
        ColType::Timestamptz,
        ColType::Uuid,
        ColType::Bytea,
        ColType::Numeric,
        ColType::Int2,
        ColType::Float4,
        ColType::Time,
        ColType::Timetz,
        ColType::Interval,
        ColType::Inet,
        ColType::Cidr,
        ColType::Macaddr,
        ColType::Macaddr8,
    ];
    let category = |t: &ColType| match t {
        ColType::Bool => "B",
        ColType::Int2
        | ColType::Int4
        | ColType::Int8
        | ColType::Float4
        | ColType::Float8
        | ColType::Numeric => "N",
        ColType::Date | ColType::Time | ColType::Timestamp | ColType::Timestamptz => "D",
        ColType::Interval => "T",
        ColType::Uuid => "U",
        ColType::Bytea => "U",
        // Network address types are PostgreSQL typcategory 'I'.
        ColType::Inet | ColType::Cidr | ColType::Macaddr | ColType::Macaddr8 => "I",
        _ => "S",
    };
    let mut out: [&[Datum]; 512 + crate::storage::MAX_DOMAINS * 2 + crate::storage::MAX_ENUMS * 2] =
        [&[]; 512 + crate::storage::MAX_DOMAINS * 2 + crate::storage::MAX_ENUMS * 2];
    for (i, t) in types.iter().enumerate() {
        out[i] = row(
            &[
                Datum::Int4(t.oid()),
                text(t.internal_name(), arena)?,
                Datum::Int4(i32::from(t.typlen())),
                Datum::Int4(0), // typcollation: none
                Datum::Int4(PG_CATALOG_NS_OID),
                text("b", arena)?,
                text(category(t), arena)?,
                Datum::Int4(0),  // typbasetype
                Datum::Int4(0),  // typelem
                Datum::Int4(0),  // typarray
                Datum::Int4(0),  // typrelid
                Datum::Int4(-1), // typtypmod
                Datum::Bool(false),
                Datum::Null, // typdefault
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(10),
                Datum::Bool(true),
                text(if t.typlen() < 0 { "x" } else { "p" }, arena)?,
            ],
            arena,
        )?;
    }
    let mut n = types.len();
    // User-defined domains: typtype 'd', with their base type and constraints.
    for slot in 0..storage.domain_count() {
        let d = storage.domain_for(slot, txid);
        if !d.visible_to(txid) {
            continue;
        }
        let base_oid = match d.base_domain {
            Some(parent) => storage
                .domain_slot(parent.schema.as_str(), parent.name.as_str(), txid)
                .map(domain_oid)
                .expect("visible domain retains its parent identity"),
            None => d.base.oid(),
        };
        let array_oid = crate::sql::types::oid::domain_array_oid(slot as u16);
        out[n] = row(
            &[
                Datum::Int4(domain_oid(slot)),
                text(
                    arena.alloc_str(d.name.as_str()).map_err(|_| arena_full())?,
                    arena,
                )?,
                Datum::Int4(i32::from(d.base.typlen())),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, d.schema.as_str())),
                text("d", arena)?,
                text(category(&d.base), arena)?,
                Datum::Int4(base_oid),
                Datum::Int4(0),
                Datum::Int4(array_oid),
                Datum::Int4(0),
                Datum::Int4(d.base_type_mod),
                Datum::Bool(d.not_null),
                match &d.default_expr {
                    Some(e) => text(
                        arena.alloc_str(e.as_str()).map_err(|_| arena_full())?,
                        arena,
                    )?,
                    None => Datum::Null,
                },
                text("", arena)?,
                text("", arena)?,
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Domain,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Domain,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text(if d.base.typlen() < 0 { "x" } else { "p" }, arena)?,
            ],
            arena,
        )?;
        n += 1;
        let array_name = crate::stack_format!(128, "_{}", d.name.as_str());
        out[n] = row(
            &[
                Datum::Int4(array_oid),
                text(
                    arena
                        .alloc_str(array_name.as_str())
                        .map_err(|_| arena_full())?,
                    arena,
                )?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, d.schema.as_str())),
                text("b", arena)?,
                text("A", arena)?,
                Datum::Int4(0),
                Datum::Int4(domain_oid(slot)),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Domain,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text("x", arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    // User-defined enum types: typtype 'e', typcategory 'E', no base type.
    for slot in 0..storage.enum_count() {
        let e = storage.enum_for(slot, txid);
        if !e.visible_to(txid) {
            continue;
        }
        let enum_oid = crate::sql::types::oid::enum_oid(slot as u16);
        let array_oid = crate::sql::types::oid::enum_array_oid(slot as u16);
        out[n] = row(
            &[
                Datum::Int4(enum_oid),
                text(
                    arena.alloc_str(e.name.as_str()).map_err(|_| arena_full())?,
                    arena,
                )?,
                Datum::Int4(4), // typlen: enums are a 4-byte oid on the wire
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, e.schema.as_str())),
                text("e", arena)?,
                text("E", arena)?,
                Datum::Int4(0), // typbasetype
                Datum::Int4(0),
                Datum::Int4(array_oid),
                Datum::Int4(0),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Enum,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Enum,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text("p", arena)?,
            ],
            arena,
        )?;
        n += 1;
        let array_name = crate::stack_format!(128, "_{}", e.name.as_str());
        out[n] = row(
            &[
                Datum::Int4(array_oid),
                text(
                    arena
                        .alloc_str(array_name.as_str())
                        .map_err(|_| arena_full())?,
                    arena,
                )?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, e.schema.as_str())),
                text("b", arena)?,
                text("A", arena)?,
                Datum::Int4(0),
                Datum::Int4(enum_oid),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Enum,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text("x", arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    // Every table, materialized view and plain view owns a composite row type.
    for slot in 0..storage.table_count() {
        let table = storage.table(slot);
        if !table.visible_to(txid) {
            continue;
        }
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_type"));
        }
        out[n] = row(
            &[
                Datum::Int4(FIRST_TABLE_COMPOSITE_TYPE_OID + slot as i32),
                text(table.def.name.as_str(), arena)?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, table.def.schema.as_str())),
                text("c", arena)?,
                text("C", arena)?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(table_oid(storage, slot)),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(
                    storage
                        .matview_slot(table.def.schema.as_str(), table.def.name.as_str(), txid)
                        .map_or(
                            owner_oid(storage, crate::storage::AccessClass::Table, slot, txid),
                            |matview| {
                                owner_oid(
                                    storage,
                                    crate::storage::AccessClass::MaterializedView,
                                    matview,
                                    txid,
                                )
                            },
                        ),
                ),
                Datum::Bool(true),
                text("x", arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    for (slot, view) in storage.views_visible_to(txid) {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_type"));
        }
        out[n] = row(
            &[
                Datum::Int4(FIRST_VIEW_COMPOSITE_TYPE_OID + slot as i32),
                text(view.name.as_str(), arena)?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, view.schema.as_str())),
                text("c", arena)?,
                text("C", arena)?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(view_oid(slot)),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::View,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text("x", arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_namespace<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_namespace",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("nspname", ColType::Text),
            ("nspowner", ColType::Int4),
            ("nspacl", ColType::Array(super::types::ArrElem::Text)),
        ],
    );
    let mut out: [&[Datum]; 2 + crate::storage::MAX_SCHEMAS] =
        [&[]; 2 + crate::storage::MAX_SCHEMAS];
    out[0] = row(
        &[
            Datum::Int4(PG_NAMESPACE_OID),
            Datum::Int4(PG_CATALOG_NS_OID),
            text("pg_catalog", arena)?,
            Datum::Int4(10),
            Datum::Null,
        ],
        arena,
    )?;
    let mut n = 1;
    for (slot, schema) in storage.visible_schemas(txid) {
        out[n] = row(
            &[
                Datum::Int4(PG_NAMESPACE_OID),
                Datum::Int4(namespace_oid(storage, schema.name.as_str())),
                text(
                    arena
                        .alloc_str(schema.name.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Schema,
                    slot,
                    txid,
                )),
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Schema,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

/// `pg_indexes`: one row per index relation, with the full `CREATE INDEX`
/// text PostgreSQL's view reconstructs. The same enumeration as `pg_class`'s
/// index rows, so psql and this view can never disagree about what exists.
fn pg_indexes<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_indexes",
        &[
            ("schemaname", ColType::Text),
            ("tablename", ColType::Text),
            ("indexname", ColType::Text),
            ("tablespace", ColType::Text),
            ("indexdef", ColType::Text),
        ],
    );
    let indices = collect_indexes(storage, txid, arena)?;
    let out = arena
        .alloc_slice_with(indices.len(), |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut n = 0;
    for info in indices {
        let table_def = storage.table_def(info.table_slot, txid);
        let mut indexdef = StackStr::<896>::new();
        {
            use core::fmt::Write as _;
            let _ = write!(
                indexdef,
                "CREATE {}INDEX {} ON {}.{} USING btree (",
                if info.is_unique { "UNIQUE " } else { "" },
                info.name.as_str(),
                table_def.schema.as_str(),
                table_def.name.as_str()
            );
            for k in 0..info.n_cols {
                if k > 0 {
                    let _ = indexdef.write_str(", ");
                }
                let _ =
                    indexdef.write_str(table_def.columns()[info.columns[k] as usize].name.as_str());
                if info.descending[k] {
                    let _ = indexdef.write_str(" DESC");
                }
                if info.nulls_first[k] != info.descending[k] {
                    let _ = indexdef.write_str(if info.nulls_first[k] {
                        " NULLS FIRST"
                    } else {
                        " NULLS LAST"
                    });
                }
            }
            let _ = indexdef.write_str(")");
        }
        out[n] = row(
            &[
                text(
                    arena
                        .alloc_str(table_def.schema.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text(table_def.name.as_str(), arena)?,
                text(
                    arena
                        .alloc_str(info.name.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                Datum::Null,
                text(
                    alloc_rendered(&indexdef, "index definition is too long", arena)?,
                    arena,
                )?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_tables<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_tables",
        &[
            ("schemaname", ColType::Text),
            ("tablename", ColType::Text),
            ("tableowner", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        if n == out.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "information_schema.tables exceeds static capacity"
            ));
        }
        out[n] = row(
            &[
                text(
                    arena
                        .alloc_str(table.schema.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text(table.name.as_str(), arena)?,
                storage
                    .matview_slot(table.schema.as_str(), table.name.as_str(), txid)
                    .map_or_else(
                        || {
                            owner_name(
                                storage,
                                crate::storage::AccessClass::Table,
                                slot,
                                txid,
                                arena,
                            )
                        },
                        |matview| {
                            owner_name(
                                storage,
                                crate::storage::AccessClass::MaterializedView,
                                matview,
                                txid,
                                arena,
                            )
                        },
                    )?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_roles<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_roles",
        &[
            ("oid", ColType::Int4),
            ("rolname", ColType::Name),
            ("rolsuper", ColType::Bool),
            ("rolinherit", ColType::Bool),
            ("rolcreaterole", ColType::Bool),
            ("rolcreatedb", ColType::Bool),
            ("rolcanlogin", ColType::Bool),
            ("rolconnlimit", ColType::Int4),
            ("rolvaliduntil", ColType::Timestamptz),
            ("rolreplication", ColType::Bool),
            ("rolbypassrls", ColType::Bool),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_ROLES + PREDEFINED_ROLES.len()] =
        [&[]; crate::storage::MAX_ROLES + PREDEFINED_ROLES.len()];
    let mut count = 0usize;
    for &(oid, name) in PREDEFINED_ROLES {
        output[count] = row(
            &[
                Datum::Int4(oid),
                text(name, arena)?,
                Datum::Bool(false),
                Datum::Bool(true),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(-1),
                Datum::Null,
                Datum::Bool(false),
                Datum::Bool(false),
            ],
            arena,
        )?;
        count += 1;
    }
    for slot in 0..storage.role_count() {
        let role = storage.role(slot);
        if !role.visible_to(txid) {
            continue;
        }
        let attributes = role.attributes_to(txid);
        let valid_until = if !attributes.has_valid_until
            || attributes
                .valid_until
                .as_str()
                .eq_ignore_ascii_case("infinity")
        {
            Datum::Null
        } else {
            Datum::Timestamptz(crate::sql::datetime::parse_timestamp(
                attributes.valid_until.as_str(),
                true,
            )?)
        };
        output[count] = row(
            &[
                Datum::Int4(Storage::role_oid(slot)),
                text(
                    arena
                        .alloc_str(role.name_to(txid).as_str())
                        .map_err(|_| arena_full())?,
                    arena,
                )?,
                Datum::Bool(attributes.superuser),
                Datum::Bool(attributes.inherit),
                Datum::Bool(attributes.create_role),
                Datum::Bool(attributes.create_database),
                Datum::Bool(attributes.can_login),
                Datum::Int4(attributes.connection_limit),
                valid_until,
                Datum::Bool(attributes.replication),
                Datum::Bool(attributes.bypass_row_level_security),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(def, &output[..count], arena)
}

fn append_base64<const N: usize>(bytes: &[u8], output: &mut StackStr<N>) {
    use core::fmt::Write;
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut at = 0usize;
    while at < bytes.len() {
        let remaining = bytes.len() - at;
        let first = bytes[at];
        let second = if remaining > 1 { bytes[at + 1] } else { 0 };
        let third = if remaining > 2 { bytes[at + 2] } else { 0 };
        let _ = output.write_char(ALPHABET[(first >> 2) as usize] as char);
        let _ =
            output.write_char(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        let _ = output.write_char(if remaining > 1 {
            ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        let _ = output.write_char(if remaining > 2 {
            ALPHABET[(third & 0x3f) as usize] as char
        } else {
            '='
        });
        at += 3;
    }
}

fn pg_authid<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let current = storage.current_role_slot(txid).ok_or_else(|| {
        sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "current role is not present in the role catalog"
        )
    })?;
    if !storage.role(current).attributes_to(txid).superuser {
        return Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied for table pg_authid"
        ));
    }
    let def = def_of(
        "pg_authid",
        &[
            ("oid", ColType::Int4),
            ("rolname", ColType::Name),
            ("rolsuper", ColType::Bool),
            ("rolinherit", ColType::Bool),
            ("rolcreaterole", ColType::Bool),
            ("rolcreatedb", ColType::Bool),
            ("rolcanlogin", ColType::Bool),
            ("rolreplication", ColType::Bool),
            ("rolbypassrls", ColType::Bool),
            ("rolconnlimit", ColType::Int4),
            ("rolpassword", ColType::Text),
            ("rolvaliduntil", ColType::Timestamptz),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_ROLES + PREDEFINED_ROLES.len()] =
        [&[]; crate::storage::MAX_ROLES + PREDEFINED_ROLES.len()];
    let mut count = 0usize;
    for &(oid, name) in PREDEFINED_ROLES {
        output[count] = row(
            &[
                Datum::Int4(oid),
                text(name, arena)?,
                Datum::Bool(false),
                Datum::Bool(true),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(-1),
                Datum::Null,
                Datum::Null,
            ],
            arena,
        )?;
        count += 1;
    }
    for slot in 0..storage.role_count() {
        let role = storage.role(slot);
        if !role.visible_to(txid) {
            continue;
        }
        let attributes = role.attributes_to(txid);
        let password = if attributes.has_password {
            use core::fmt::Write;
            let mut verifier = StackStr::<192>::new();
            let _ = write!(
                verifier,
                "SCRAM-SHA-256${}:",
                attributes.password.iterations
            );
            append_base64(&attributes.password.salt, &mut verifier);
            let _ = verifier.write_char('$');
            append_base64(&attributes.password.stored_key, &mut verifier);
            let _ = verifier.write_char(':');
            append_base64(&attributes.password.server_key, &mut verifier);
            if verifier.is_truncated() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "SCRAM verifier exceeds catalog rendering limit"
                ));
            }
            Datum::Text(
                arena
                    .alloc_str(verifier.as_str())
                    .map_err(|_| arena_full())?,
            )
        } else {
            Datum::Null
        };
        let valid_until = if !attributes.has_valid_until
            || attributes
                .valid_until
                .as_str()
                .eq_ignore_ascii_case("infinity")
        {
            Datum::Null
        } else {
            Datum::Timestamptz(crate::sql::datetime::parse_timestamp(
                attributes.valid_until.as_str(),
                true,
            )?)
        };
        output[count] = row(
            &[
                Datum::Int4(Storage::role_oid(slot)),
                text(role.name_to(txid).as_str(), arena)?,
                Datum::Bool(attributes.superuser),
                Datum::Bool(attributes.inherit),
                Datum::Bool(attributes.create_role),
                Datum::Bool(attributes.create_database),
                Datum::Bool(attributes.can_login),
                Datum::Bool(attributes.replication),
                Datum::Bool(attributes.bypass_row_level_security),
                Datum::Int4(attributes.connection_limit),
                password,
                valid_until,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(def, &output[..count], arena)
}

fn pg_auth_members<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_auth_members",
        &[
            ("roleid", ColType::Int4),
            ("member", ColType::Int4),
            ("grantor", ColType::Int4),
            ("admin_option", ColType::Bool),
            ("inherit_option", ColType::Bool),
            ("set_option", ColType::Bool),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_ROLE_MEMBERSHIPS] =
        [&[]; crate::storage::MAX_ROLE_MEMBERSHIPS];
    let mut count = 0usize;
    for slot in 0..storage.role_membership_count() {
        let membership = storage.role_membership(slot);
        if !membership.visible_to(txid) {
            continue;
        }
        let options = membership.options_to(txid);
        output[count] = row(
            &[
                Datum::Int4(Storage::role_oid(membership.role as usize)),
                Datum::Int4(Storage::role_oid(membership.member as usize)),
                Datum::Int4(Storage::role_oid(membership.grantor as usize)),
                Datum::Bool(options.admin),
                Datum::Bool(options.inherit),
                Datum::Bool(options.set),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(def, &output[..count], arena)
}

fn pg_views<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_views",
        &[
            ("schemaname", ColType::Name),
            ("viewname", ColType::Name),
            ("viewowner", ColType::Name),
            ("definition", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for slot in 0..storage.view_count() {
        let view = storage.view(slot);
        if !view.visible_to(txid) || n == out.len() {
            continue;
        }
        out[n] = row(
            &[
                text(view.schema.as_str(), arena)?,
                text(view.name.as_str(), arena)?,
                owner_name(
                    storage,
                    crate::storage::AccessClass::View,
                    slot,
                    txid,
                    arena,
                )?,
                text(view.sql.as_str(), arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_matviews<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_matviews",
        &[
            ("schemaname", ColType::Name),
            ("matviewname", ColType::Name),
            ("matviewowner", ColType::Name),
            ("tablespace", ColType::Name),
            ("hasindexes", ColType::Bool),
            ("ispopulated", ColType::Bool),
            ("definition", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for slot in 0..storage.matview_count() {
        let mv = storage.matview(slot);
        if !mv.visible_to(txid) || n == out.len() {
            continue;
        }
        out[n] = row(
            &[
                text(
                    arena
                        .alloc_str(mv.schema.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text(
                    arena
                        .alloc_str(mv.name.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                owner_name(
                    storage,
                    crate::storage::AccessClass::MaterializedView,
                    slot,
                    txid,
                    arena,
                )?,
                Datum::Null,
                Datum::Bool(false),
                Datum::Bool(mv.populated),
                text(
                    arena
                        .alloc_str(mv.sql.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_sequences<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_sequences",
        &[
            ("schemaname", ColType::Text),
            ("sequencename", ColType::Text),
            ("sequenceowner", ColType::Text),
            ("data_type", ColType::Text),
            ("start_value", ColType::Int8),
            ("min_value", ColType::Int8),
            ("max_value", ColType::Int8),
            ("increment_by", ColType::Int8),
            ("cycle", ColType::Bool),
            ("cache_size", ColType::Int8),
            ("last_value", ColType::Int8),
        ],
    );
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for slot in 0..storage.sequence_count() {
        let seq = storage.sequence_for(slot, txid);
        if !seq.visible_to(txid) {
            continue;
        }
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_sequences"));
        }
        // last_value is NULL until the sequence has been advanced at least once,
        // exactly as PostgreSQL reports it.
        let (sequence_last_value, sequence_is_called) = storage.sequence_value_for(slot, txid);
        let last_value = if sequence_is_called {
            Datum::Int8(sequence_last_value)
        } else {
            Datum::Null
        };
        out[n] = row(
            &[
                text(seq.schema.as_str(), arena)?,
                text(seq.name.as_str(), arena)?,
                owner_name(
                    storage,
                    crate::storage::AccessClass::Sequence,
                    slot,
                    txid,
                    arena,
                )?,
                text(seq.data_type.sql_name(), arena)?,
                Datum::Int8(seq.start_value),
                Datum::Int8(seq.min_value),
                Datum::Int8(seq.max_value),
                Datum::Int8(seq.increment),
                Datum::Bool(seq.cycle),
                Datum::Int8(seq.cache),
                last_value,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_sequence<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_sequence",
        &[
            ("seqrelid", ColType::Int4),
            ("seqtypid", ColType::Int4),
            ("seqstart", ColType::Int8),
            ("seqincrement", ColType::Int8),
            ("seqmax", ColType::Int8),
            ("seqmin", ColType::Int8),
            ("seqcache", ColType::Int8),
            ("seqcycle", ColType::Bool),
        ],
    );
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for slot in 0..storage.sequence_count() {
        let seq = storage.sequence_for(slot, txid);
        if !seq.visible_to(txid) {
            continue;
        }
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_sequence"));
        }
        out[n] = row(
            &[
                Datum::Int4(sequence_oid(slot)),
                Datum::Int4(seq.data_type.oid()),
                Datum::Int8(seq.start_value),
                Datum::Int8(seq.increment),
                Datum::Int8(seq.max_value),
                Datum::Int8(seq.min_value),
                Datum::Int8(seq.cache),
                Datum::Bool(seq.cycle),
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn info_tables<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "tables",
        &[
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("table_type", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        if n == out.len() {
            return Err(catalog_capacity_exceeded("information_schema.tables"));
        }
        out[n] = row(
            &[
                text("postgres", arena)?,
                text(
                    arena
                        .alloc_str(table.schema.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text(table.name.as_str(), arena)?,
                text("BASE TABLE", arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    for (_, view) in storage.views_visible_to(txid) {
        if n == out.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "information_schema.tables exceeds static capacity"
            ));
        }
        out[n] = row(
            &[
                text("postgres", arena)?,
                text(
                    arena
                        .alloc_str(view.schema.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text(
                    arena
                        .alloc_str(view.name.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text("VIEW", arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn routine_specific_name(
    routine: &crate::storage::RoutineDef,
    txid: u32,
) -> crate::util::StackStr<96> {
    use core::fmt::Write;
    let mut name = crate::util::StackStr::new();
    let _ = write!(
        name,
        "{}_{}",
        routine.name_for(txid).as_str(),
        crate::storage::routine_oid(routine)
    );
    name
}

fn info_routines<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "routines",
        &[
            ("specific_catalog", ColType::Text),
            ("specific_schema", ColType::Text),
            ("specific_name", ColType::Text),
            ("routine_catalog", ColType::Text),
            ("routine_schema", ColType::Text),
            ("routine_name", ColType::Text),
            ("routine_type", ColType::Text),
            ("data_type", ColType::Text),
            ("external_language", ColType::Text),
            ("routine_definition", ColType::Text),
        ],
    );
    let count = (0..storage.routine_count())
        .filter(|slot| storage.routine(*slot).visible_to(txid))
        .count();
    let output = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut row_index = 0;
    for slot in 0..storage.routine_count() {
        let routine = storage.routine(slot);
        if !routine.visible_to(txid) {
            continue;
        }
        let specific_name = routine_specific_name(routine, txid);
        output[row_index] = row(
            &[
                text("postgres", arena)?,
                text(routine.schema_for(txid).as_str(), arena)?,
                text(specific_name.as_str(), arena)?,
                text("postgres", arena)?,
                text(routine.schema_for(txid).as_str(), arena)?,
                text(routine.name_for(txid).as_str(), arena)?,
                text(
                    if matches!(routine.kind, crate::storage::RoutineKind::Procedure) {
                        "PROCEDURE"
                    } else {
                        "FUNCTION"
                    },
                    arena,
                )?,
                match routine.kind.function_result() {
                    Some(result) => text(result.name(), arena)?,
                    None => Datum::Null,
                },
                text("SQL", arena)?,
                text(routine.body.as_str(), arena)?,
            ],
            arena,
        )?;
        row_index += 1;
    }
    finish(definition, output, arena)
}

fn info_routine_privileges<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    include_public: bool,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        if include_public {
            "routine_privileges"
        } else {
            "role_routine_grants"
        },
        &[
            ("grantor", ColType::Text),
            ("grantee", ColType::Text),
            ("specific_catalog", ColType::Text),
            ("specific_schema", ColType::Text),
            ("specific_name", ColType::Text),
            ("routine_catalog", ColType::Text),
            ("routine_schema", ColType::Text),
            ("routine_name", ColType::Text),
            ("privilege_type", ColType::Text),
            ("is_grantable", ColType::Text),
        ],
    );
    let capacity = storage.routine_count() + crate::storage::MAX_ACL_ENTRIES;
    let output = arena
        .alloc_slice_with(capacity, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0usize;
    let mut append =
        |slot: usize, grantor: u16, grantee: u16, grantable: bool| -> Result<(), SqlError> {
            if (!include_public && grantee == crate::storage::PUBLIC_ROLE)
                || (!storage.role_is_enabled(grantor, txid)
                    && !storage.role_is_enabled(grantee, txid))
            {
                return Ok(());
            }
            if count == output.len() {
                return Err(catalog_capacity_exceeded(
                    "information_schema.routine_privileges",
                ));
            }
            let routine = storage.routine(slot);
            let specific_name = routine_specific_name(routine, txid);
            let grantor_name = storage.role_name(grantor as usize, txid);
            let grantee_name = if grantee == crate::storage::PUBLIC_ROLE {
                SqlName::parse("PUBLIC").expect("PUBLIC fits a SQL name")
            } else {
                storage.role_name(grantee as usize, txid)
            };
            output[count] = row(
                &[
                    text(grantor_name.as_str(), arena)?,
                    text(grantee_name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(routine.schema_for(txid).as_str(), arena)?,
                    text(specific_name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(routine.schema_for(txid).as_str(), arena)?,
                    text(routine.name_for(txid).as_str(), arena)?,
                    text("EXECUTE", arena)?,
                    text(if grantable { "YES" } else { "NO" }, arena)?,
                ],
                arena,
            )?;
            count += 1;
            Ok(())
        };
    for slot in 0..storage.routine_count() {
        let routine = storage.routine(slot);
        if !routine.visible_to(txid) {
            continue;
        }
        let object = Storage::routine_access_object(slot);
        let owner = storage.object_owner(object, txid) as u16;
        append(slot, owner, owner, true)?;
        let public_defined = storage.acl_entries().any(|(acl_slot, entry)| {
            entry.object == object
                && storage.acl_identity(acl_slot, txid).0 == crate::storage::PUBLIC_ROLE
        });
        if !public_defined {
            append(slot, owner, crate::storage::PUBLIC_ROLE, false)?;
        }
        for (acl_slot, entry) in storage.acl_entries() {
            if entry.object != object {
                continue;
            }
            let (privileges, options) = storage.acl_state(acl_slot, txid);
            if !privileges.contains(crate::storage::PrivilegeSet::EXECUTE) {
                continue;
            }
            let (grantee, grantor) = storage.acl_identity(acl_slot, txid);
            if grantee == owner && grantor == owner {
                continue;
            }
            append(
                slot,
                grantor,
                grantee,
                options.contains(crate::storage::PrivilegeSet::EXECUTE),
            )?;
        }
    }
    finish(definition, &output[..count], arena)
}

fn info_parameters<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "parameters",
        &[
            ("specific_catalog", ColType::Text),
            ("specific_schema", ColType::Text),
            ("specific_name", ColType::Text),
            ("ordinal_position", ColType::Int4),
            ("parameter_mode", ColType::Text),
            ("parameter_name", ColType::Text),
            ("data_type", ColType::Text),
            ("udt_catalog", ColType::Text),
            ("udt_schema", ColType::Text),
            ("udt_name", ColType::Text),
        ],
    );
    let count = (0..storage.routine_count())
        .filter(|slot| storage.routine(*slot).visible_to(txid))
        .map(|slot| storage.routine(slot).argument_count)
        .sum();
    let output = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut row_index = 0;
    for slot in 0..storage.routine_count() {
        let routine = storage.routine(slot);
        if !routine.visible_to(txid) {
            continue;
        }
        let specific_name = routine_specific_name(routine, txid);
        for (argument_index, argument) in routine.arguments().iter().enumerate() {
            output[row_index] = row(
                &[
                    text("postgres", arena)?,
                    text(routine.schema_for(txid).as_str(), arena)?,
                    text(specific_name.as_str(), arena)?,
                    Datum::Int4((argument_index + 1) as i32),
                    text("IN", arena)?,
                    if argument.name.as_str().is_empty() {
                        Datum::Null
                    } else {
                        text(argument.name.as_str(), arena)?
                    },
                    text(argument.ctype.name(), arena)?,
                    text("postgres", arena)?,
                    text("pg_catalog", arena)?,
                    text(argument.ctype.name(), arena)?,
                ],
                arena,
            )?;
            row_index += 1;
        }
    }
    finish(definition, output, arena)
}

fn info_views<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "views",
        &[
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("view_definition", ColType::Text),
            ("check_option", ColType::Text),
            ("is_updatable", ColType::Text),
            ("is_insertable_into", ColType::Text),
            ("is_trigger_updatable", ColType::Text),
            ("is_trigger_deletable", ColType::Text),
            ("is_trigger_insertable_into", ColType::Text),
        ],
    );
    let count = storage.views_visible_to(txid).count();
    let out = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (_, view)) in storage.views_visible_to(txid).enumerate() {
        let is_updatable = super::query::view_is_auto_updatable(
            storage,
            view.schema.as_str(),
            view.name.as_str(),
            txid,
            arena,
        )?;
        let writable = if is_updatable { "YES" } else { "NO" };
        out[index] = row(
            &[
                text("postgres", arena)?,
                text(view.schema.as_str(), arena)?,
                text(view.name.as_str(), arena)?,
                text(view.sql.as_str(), arena)?,
                text("NONE", arena)?,
                text(writable, arena)?,
                text(writable, arena)?,
                text("NO", arena)?,
                text("NO", arena)?,
                text("NO", arena)?,
            ],
            arena,
        )?;
    }
    finish(def, out, arena)
}

fn info_view_table_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "view_table_usage",
        &[
            ("view_catalog", ColType::Text),
            ("view_schema", ColType::Text),
            ("view_name", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
        ],
    );
    let mut count = 0usize;
    for (view_slot, _) in storage.views_visible_to(txid) {
        for dependency in storage.view_dependencies(view_slot).entries() {
            if matches!(
                dependency.class,
                crate::storage::DependencyClass::Table | crate::storage::DependencyClass::View
            ) {
                count = count.checked_add(1).ok_or_else(|| {
                    catalog_capacity_exceeded("information_schema.view_table_usage")
                })?;
            }
        }
    }
    let out = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut index = 0usize;
    for (view_slot, view) in storage.views_visible_to(txid) {
        for dependency in storage.view_dependencies(view_slot).entries() {
            let (schema, name) = match dependency.class {
                crate::storage::DependencyClass::Table => {
                    let slot = dependency.slot as usize;
                    if !storage.table(slot).visible_to(txid) {
                        return Err(sql_err!(
                            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                            "view \"{}\" has a stale table dependency",
                            view.name.as_str()
                        ));
                    }
                    let table = storage.table_def(slot, txid);
                    (table.schema.as_str(), table.name.as_str())
                }
                crate::storage::DependencyClass::View => {
                    let slot = dependency.slot as usize;
                    let source = storage.view(slot);
                    if !source.visible_to(txid) {
                        return Err(sql_err!(
                            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                            "view \"{}\" has a stale view dependency",
                            view.name.as_str()
                        ));
                    }
                    (source.schema.as_str(), source.name.as_str())
                }
                _ => continue,
            };
            out[index] = row(
                &[
                    text("postgres", arena)?,
                    text(view.schema.as_str(), arena)?,
                    text(view.name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(schema, arena)?,
                    text(name, arena)?,
                ],
                arena,
            )?;
            index += 1;
        }
    }
    debug_assert_eq!(index, out.len());
    finish(def, out, arena)
}

fn info_view_column_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "view_column_usage",
        &[
            ("view_catalog", ColType::Text),
            ("view_schema", ColType::Text),
            ("view_name", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("column_name", ColType::Text),
        ],
    );
    let mut count = 0usize;
    for (view_slot, view) in storage.views_visible_to(txid) {
        for dependency in storage.view_dependencies(view_slot).entries() {
            if dependency.class == crate::storage::DependencyClass::Table {
                let table_slot = dependency.slot as usize;
                if !storage.table(table_slot).visible_to(txid) {
                    return Err(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "view \"{}\" has a stale table dependency",
                        view.name.as_str()
                    ));
                }
                let columns = storage.table_def(table_slot, txid).columns().len();
                let valid_mask = if columns == u64::BITS as usize {
                    u64::MAX
                } else {
                    (1u64 << columns) - 1
                };
                if dependency.referenced_columns & !valid_mask != 0 {
                    return Err(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "view \"{}\" has a stale column dependency",
                        view.name.as_str()
                    ));
                }
                count = count
                    .checked_add(dependency.referenced_columns.count_ones() as usize)
                    .ok_or_else(|| {
                        catalog_capacity_exceeded("information_schema.view_column_usage")
                    })?;
            } else if dependency.class == crate::storage::DependencyClass::View {
                let source_slot = dependency.slot as usize;
                let source = storage.view(source_slot);
                if !source.visible_to(txid) {
                    return Err(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "view \"{}\" has a stale view dependency",
                        view.name.as_str()
                    ));
                }
                let mut columns =
                    [super::types::ColDesc::new("", 0, 0); crate::storage::MAX_COLUMNS];
                let n_columns =
                    describe_stored_view(storage, txid, source_slot, arena, &mut columns)?;
                let valid_mask = if n_columns == u64::BITS as usize {
                    u64::MAX
                } else {
                    (1u64 << n_columns) - 1
                };
                if dependency.referenced_columns & !valid_mask != 0 {
                    return Err(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "view \"{}\" has a stale column dependency",
                        view.name.as_str()
                    ));
                }
                count = count
                    .checked_add(dependency.referenced_columns.count_ones() as usize)
                    .ok_or_else(|| {
                        catalog_capacity_exceeded("information_schema.view_column_usage")
                    })?;
            }
        }
    }
    let out = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut index = 0usize;
    for (view_slot, view) in storage.views_visible_to(txid) {
        for dependency in storage.view_dependencies(view_slot).entries() {
            match dependency.class {
                crate::storage::DependencyClass::Table => {
                    let table = storage.table_def(dependency.slot as usize, txid);
                    for column in 0..table.columns().len() {
                        if dependency.referenced_columns & (1u64 << column) != 0 {
                            out[index] = row(
                                &[
                                    text("postgres", arena)?,
                                    text(view.schema.as_str(), arena)?,
                                    text(view.name.as_str(), arena)?,
                                    text("postgres", arena)?,
                                    text(table.schema.as_str(), arena)?,
                                    text(table.name.as_str(), arena)?,
                                    text(table.columns()[column].name.as_str(), arena)?,
                                ],
                                arena,
                            )?;
                            index += 1;
                        }
                    }
                }
                crate::storage::DependencyClass::View => {
                    let source_slot = dependency.slot as usize;
                    let source = storage.view(source_slot);
                    let mut columns =
                        [super::types::ColDesc::new("", 0, 0); crate::storage::MAX_COLUMNS];
                    let n_columns =
                        describe_stored_view(storage, txid, source_slot, arena, &mut columns)?;
                    for (column, descriptor) in columns.iter().enumerate().take(n_columns) {
                        if dependency.referenced_columns & (1u64 << column) != 0 {
                            out[index] = row(
                                &[
                                    text("postgres", arena)?,
                                    text(view.schema.as_str(), arena)?,
                                    text(view.name.as_str(), arena)?,
                                    text("postgres", arena)?,
                                    text(source.schema.as_str(), arena)?,
                                    text(source.name.as_str(), arena)?,
                                    text(descriptor.name, arena)?,
                                ],
                                arena,
                            )?;
                            index += 1;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    debug_assert_eq!(index, out.len());
    finish(def, out, arena)
}

fn info_columns<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "columns",
        &[
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("column_name", ColType::Text),
            ("ordinal_position", ColType::Int4),
            ("column_default", ColType::Text),
            ("is_nullable", ColType::Text),
            ("data_type", ColType::Text),
            ("character_maximum_length", ColType::Int4),
            ("character_octet_length", ColType::Int4),
            ("numeric_precision", ColType::Int4),
            ("numeric_precision_radix", ColType::Int4),
            ("numeric_scale", ColType::Int4),
            ("datetime_precision", ColType::Int4),
            ("interval_type", ColType::Text),
            ("interval_precision", ColType::Int4),
            ("character_set_catalog", ColType::Text),
            ("character_set_schema", ColType::Text),
            ("character_set_name", ColType::Text),
            ("collation_catalog", ColType::Text),
            ("collation_schema", ColType::Text),
            ("collation_name", ColType::Text),
            ("domain_catalog", ColType::Text),
            ("domain_schema", ColType::Text),
            ("domain_name", ColType::Text),
            ("udt_catalog", ColType::Text),
            ("udt_schema", ColType::Text),
            ("udt_name", ColType::Text),
            ("scope_catalog", ColType::Text),
            ("scope_schema", ColType::Text),
            ("scope_name", ColType::Text),
            ("maximum_cardinality", ColType::Int4),
            ("dtd_identifier", ColType::Text),
            ("is_self_referencing", ColType::Text),
            ("is_identity", ColType::Text),
            ("identity_generation", ColType::Text),
            ("identity_start", ColType::Text),
            ("identity_increment", ColType::Text),
            ("identity_maximum", ColType::Text),
            ("identity_minimum", ColType::Text),
            ("identity_cycle", ColType::Text),
            ("is_generated", ColType::Text),
            ("generation_expression", ColType::Text),
            ("is_updatable", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 1024] = [&[]; 1024];
    let mut n = 0;
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for (i, c) in table.columns().iter().enumerate() {
            if n == out.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "information_schema.columns exceeds static capacity"
                ));
            }
            out[n] = info_column_row(
                storage,
                txid,
                InformationSchemaColumnSource {
                    schema: table.schema.as_str(),
                    table: table.name.as_str(),
                    name: c.name.as_str(),
                    position: i + 1,
                    column: c,
                    updatable: true,
                },
                arena,
            )?;
            n += 1;
        }
    }
    // A view's row type is derived from its SELECT under the path captured at
    // CREATE VIEW time. Reusing that resolver keeps information_schema,
    // pg_attribute, Describe, and execution on one source of truth.
    for (_, view) in storage.views_visible_to(txid) {
        let mut columns = [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
        let count = describe_view(storage, txid, view, arena, &mut columns)?;
        for (index, column) in columns[..count].iter().enumerate() {
            if n == out.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "information_schema.columns exceeds static capacity"
                ));
            }
            let (ctype, user_type) = view_column_catalog_type(storage, txid, column.type_oid)?;
            let column_meta = ColumnMeta {
                name: SqlName::EMPTY,
                ctype,
                type_mod: column.type_mod,
                not_null: false,
                unique: false,
                primary: false,
                auto_increment: false,
                default: crate::storage::ColumnDefault::NONE,
                is_identity: false,
                identity_always: false,
                auto_increment_step: 1,
                user_type,
            };
            out[n] = info_column_row(
                storage,
                txid,
                InformationSchemaColumnSource {
                    schema: view.schema.as_str(),
                    table: view.name.as_str(),
                    name: column.name,
                    position: index + 1,
                    column: &column_meta,
                    updatable: false,
                },
                arena,
            )?;
            n += 1;
        }
    }
    finish(def, &out[..n], arena)
}

fn view_column_catalog_type(
    storage: &Storage,
    txid: u32,
    oid: i32,
) -> Result<(ColType, Option<crate::storage::UserTypeName>), SqlError> {
    use crate::sql::types::oid as type_oid;
    if (type_oid::FIRST_DOMAIN..type_oid::FIRST_DOMAIN + crate::storage::MAX_DOMAINS as i32)
        .contains(&oid)
    {
        let definition = storage.domain_for((oid - type_oid::FIRST_DOMAIN) as usize, txid);
        return Ok((
            definition.base,
            Some(crate::storage::UserTypeName {
                schema: definition.schema,
                name: definition.name,
            }),
        ));
    }
    if (type_oid::FIRST_ENUM..type_oid::FIRST_ENUM + crate::storage::MAX_ENUMS as i32)
        .contains(&oid)
    {
        let definition = storage.enum_for((oid - type_oid::FIRST_ENUM) as usize, txid);
        return Ok((
            ColType::Enum((oid - type_oid::FIRST_ENUM) as u16),
            Some(crate::storage::UserTypeName {
                schema: definition.schema,
                name: definition.name,
            }),
        ));
    }
    super::exec::coltype_of_oid(oid)
        .map(|ctype| (ctype, None))
        .ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "view column has unsupported type oid {}",
                oid
            )
        })
}

/// The type-dependent portion of `information_schema.columns`.  `TypeMod`
/// keeps the several PostgreSQL wire encodings from leaking into catalog code.
struct InformationSchemaColumnSource<'a> {
    schema: &'a str,
    table: &'a str,
    name: &'a str,
    position: usize,
    column: &'a ColumnMeta,
    updatable: bool,
}

fn info_column_row<'a>(
    storage: &Storage,
    txid: u32,
    source: InformationSchemaColumnSource<'_>,
    arena: &'a Arena,
) -> Result<&'a [Datum<'a>], SqlError> {
    use crate::sql::types::oid;
    let InformationSchemaColumnSource {
        schema,
        table,
        name,
        position,
        column,
        updatable,
    } = source;
    let declared_oid = catalog_column_type_oid(storage, column, txid)?;
    let is_domain = (oid::FIRST_DOMAIN..oid::FIRST_DOMAIN + crate::storage::MAX_DOMAINS as i32)
        .contains(&declared_oid);
    let domain =
        is_domain.then(|| storage.domain_for((declared_oid - oid::FIRST_DOMAIN) as usize, txid));
    let type_mod = TypeMod::decode(column.ctype, column.type_mod);
    let data_type = information_schema_data_type(storage, txid, declared_oid)?;
    let (character_length, numeric_precision, numeric_radix, numeric_scale, datetime_precision) =
        match type_mod {
            TypeMod::Length(length)
                if matches!(column.ctype, ColType::Varchar | ColType::Bpchar) =>
            {
                (Some(length as i32), None, None, None, None)
            }
            TypeMod::NumericPS { precision, scale } => (
                None,
                Some(precision as i32),
                Some(10),
                Some(scale as i32),
                None,
            ),
            TypeMod::TemporalPrecision(precision) => {
                (None, None, None, None, Some(precision as i32))
            }
            TypeMod::IntervalMod { precision, .. } => {
                (None, None, None, None, precision.map(i32::from))
            }
            _ => match column.ctype {
                ColType::Int2 => (None, Some(16), Some(2), Some(0), None),
                ColType::Int4 => (None, Some(32), Some(2), Some(0), None),
                ColType::Int8 => (None, Some(64), Some(2), Some(0), None),
                ColType::Float4 => (None, Some(24), Some(2), None, None),
                ColType::Float8 => (None, Some(53), Some(2), None, None),
                _ => (None, None, None, None, None),
            },
        };
    let user_type = column.user_type;
    let (udt_schema, udt_name) = if let Some(identity) = user_type {
        let mut type_name = StackStr::<64>::new();
        if matches!(column.ctype, ColType::Array(_)) {
            use core::fmt::Write as _;
            let _ = write!(type_name, "_{}", identity.name.as_str());
        } else {
            use core::fmt::Write as _;
            let _ = write!(type_name, "{}", identity.name.as_str());
        }
        (identity.schema, type_name)
    } else {
        (
            SqlName::parse("pg_catalog").expect("catalog schema fits"),
            StackStr::from_str(column.ctype.catalog_name()),
        )
    };
    let default = (!column.default.is_generated())
        .then(|| column.default.expression())
        .flatten();
    let generated = column.default.is_generated();
    let generated_expression = generated.then(|| column.default.expression()).flatten();
    let nullable = !column.not_null && !domain.is_some_and(|definition| definition.not_null);
    let identity = column.is_identity;
    let identity_sequence = identity
        .then(|| {
            (0..storage.sequence_count())
                .map(|slot| storage.sequence_for(slot, txid))
                .find(|sequence| {
                    sequence.generator_for.is_some_and(|owner| {
                        owner.table_schema.as_str() == schema
                            && owner.table.as_str() == table
                            && owner.column.as_str() == name
                    })
                })
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "identity column \"{}.{}.{}\" has no generator sequence",
                        schema,
                        table,
                        name
                    )
                })
        })
        .transpose()?;
    let identity_start = identity_sequence
        .as_ref()
        .map(|sequence| stack_format!(32, "{}", sequence.start_value));
    let identity_increment = identity_sequence
        .as_ref()
        .map(|sequence| stack_format!(32, "{}", sequence.increment));
    let identity_maximum = identity_sequence
        .as_ref()
        .map(|sequence| stack_format!(32, "{}", sequence.max_value));
    let identity_minimum = identity_sequence
        .as_ref()
        .map(|sequence| stack_format!(32, "{}", sequence.min_value));
    let identity_cycle = identity_sequence.as_ref().map(|sequence| sequence.cycle);
    let default_datum = match default {
        Some(value) => text(value.as_str(), arena)?,
        None => Datum::Null,
    };
    let domain_catalog = match domain {
        Some(_) => text("postgres", arena)?,
        None => Datum::Null,
    };
    let domain_schema = match domain {
        Some(value) => text(value.schema.as_str(), arena)?,
        None => Datum::Null,
    };
    let domain_name = match domain {
        Some(value) => text(value.name.as_str(), arena)?,
        None => Datum::Null,
    };
    let generated_expression = match generated_expression {
        Some(value) => text(value.as_str(), arena)?,
        None => Datum::Null,
    };
    row(
        &[
            text("postgres", arena)?,
            text(schema, arena)?,
            text(table, arena)?,
            text(name, arena)?,
            Datum::Int4(position as i32),
            default_datum,
            text(if nullable { "YES" } else { "NO" }, arena)?,
            text(data_type.as_str(), arena)?,
            character_length.map_or(Datum::Null, Datum::Int4),
            character_length.map_or(Datum::Null, Datum::Int4),
            numeric_precision.map_or(Datum::Null, Datum::Int4),
            numeric_radix.map_or(Datum::Null, Datum::Int4),
            numeric_scale.map_or(Datum::Null, Datum::Int4),
            datetime_precision.map_or(Datum::Null, Datum::Int4),
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            domain_catalog,
            domain_schema,
            domain_name,
            text("postgres", arena)?,
            text(udt_schema.as_str(), arena)?,
            text(udt_name.as_str(), arena)?,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            text(stack_format!(32, "{}", position).as_str(), arena)?,
            text("NO", arena)?,
            text(if identity { "YES" } else { "NO" }, arena)?,
            if identity {
                text(
                    if column.identity_always {
                        "ALWAYS"
                    } else {
                        "BY DEFAULT"
                    },
                    arena,
                )?
            } else {
                Datum::Null
            },
            if identity {
                text(
                    identity_start.expect("identity has a sequence").as_str(),
                    arena,
                )?
            } else {
                Datum::Null
            },
            if identity {
                text(
                    identity_increment
                        .expect("identity has a sequence")
                        .as_str(),
                    arena,
                )?
            } else {
                Datum::Null
            },
            if identity {
                text(
                    identity_maximum.expect("identity has a sequence").as_str(),
                    arena,
                )?
            } else {
                Datum::Null
            },
            if identity {
                text(
                    identity_minimum.expect("identity has a sequence").as_str(),
                    arena,
                )?
            } else {
                Datum::Null
            },
            text(
                if identity_cycle == Some(true) {
                    "YES"
                } else {
                    "NO"
                },
                arena,
            )?,
            text(if generated { "ALWAYS" } else { "NEVER" }, arena)?,
            generated_expression,
            text(if updatable { "YES" } else { "NO" }, arena)?,
        ],
        arena,
    )
}

/// The SQL-standard `data_type` spelling is intentionally less specific than
/// PostgreSQL's OID. Domains report their base type, enums report
/// `USER-DEFINED`, and arrays report `ARRAY`.
fn information_schema_data_type(
    storage: &Storage,
    txid: u32,
    oid: i32,
) -> Result<StackStr<64>, SqlError> {
    use crate::sql::types::oid as type_oid;
    if (type_oid::FIRST_DOMAIN..type_oid::FIRST_DOMAIN + crate::storage::MAX_DOMAINS as i32)
        .contains(&oid)
    {
        return Ok(StackStr::from_str(
            storage
                .domain_for((oid - type_oid::FIRST_DOMAIN) as usize, txid)
                .base
                .name(),
        ));
    }
    if (type_oid::FIRST_DOMAIN_ARRAY
        ..type_oid::FIRST_DOMAIN_ARRAY + crate::storage::MAX_DOMAINS as i32)
        .contains(&oid)
        || (type_oid::FIRST_ENUM_ARRAY
            ..type_oid::FIRST_ENUM_ARRAY + crate::storage::MAX_ENUMS as i32)
            .contains(&oid)
    {
        return Ok(StackStr::from_str("ARRAY"));
    }
    if (type_oid::FIRST_ENUM..type_oid::FIRST_ENUM + crate::storage::MAX_ENUMS as i32)
        .contains(&oid)
    {
        return Ok(StackStr::from_str("USER-DEFINED"));
    }
    if let Some(ctype) = super::exec::coltype_of_oid(oid) {
        if matches!(ctype, ColType::Array(_)) {
            return Ok(StackStr::from_str("ARRAY"));
        }
        return Ok(StackStr::from_str(ctype.name()));
    }
    Err(sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "information_schema.columns cannot describe type oid {}",
        oid
    ))
}

fn info_table_constraints<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "table_constraints",
        &[
            ("constraint_catalog", ColType::Text),
            ("constraint_schema", ColType::Text),
            ("constraint_name", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("constraint_type", ColType::Text),
            ("is_deferrable", ColType::Text),
            ("initially_deferred", ColType::Text),
            ("enforced", ColType::Text),
            ("nulls_distinct", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = crate::sql::query::MAX_JOIN_TABLES
        * (crate::storage::MAX_COLUMNS * 2
            + crate::storage::MAX_UNIQUES
            + crate::storage::MAX_CHECKS
            + crate::storage::MAX_FKEYS);
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0;
    let mut append = |name: &str,
                      kind: &str,
                      nulls_distinct: Option<&str>,
                      table: &TableDef|
     -> Result<(), SqlError> {
        if count == output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table_constraints exceeds static capacity"
            ));
        }
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(table.schema.as_str(), arena)?,
                text(name, arena)?,
                text("postgres", arena)?,
                text(table.schema.as_str(), arena)?,
                text(table.name.as_str(), arena)?,
                text(kind, arena)?,
                text("NO", arena)?,
                text("NO", arena)?,
                text("YES", arena)?,
                nulls_distinct.map_or(Ok(Datum::Null), |value| text(value, arena))?,
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for column in table.columns() {
            if column.primary {
                let name = inline_primary_constraint_name(table);
                append(name.as_str(), "PRIMARY KEY", None, table)?;
            } else if column.unique {
                let name = inline_unique_constraint_name(table, column);
                append(name.as_str(), "UNIQUE", Some("YES"), table)?;
            }
            if column.not_null {
                let name = not_null_constraint_name(table, column);
                append(name.as_str(), "CHECK", None, table)?;
            }
        }
        for unique in table.uniques() {
            append(
                unique.name.as_str(),
                if unique.is_primary {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                },
                (!unique.is_primary).then_some("YES"),
                table,
            )?;
        }
        for check in table.checks() {
            append(check.name.as_str(), "CHECK", None, table)?;
        }
        for foreign_key in table.fkeys() {
            append(foreign_key.name.as_str(), "FOREIGN KEY", None, table)?;
        }
    }
    finish(definition, &output[..count], arena)
}

fn inline_primary_constraint_name(table: &TableDef) -> StackStr<128> {
    stack_format!(128, "{}_pkey", table.name.as_str())
}

fn inline_unique_constraint_name(table: &TableDef, column: &ColumnMeta) -> StackStr<128> {
    stack_format!(128, "{}_{}_key", table.name.as_str(), column.name.as_str())
}

fn not_null_constraint_name(table: &TableDef, column: &ColumnMeta) -> StackStr<128> {
    stack_format!(
        128,
        "{}_{}_not_null",
        table.name.as_str(),
        column.name.as_str()
    )
}

struct KeyConstraint {
    name: StackStr<128>,
    columns: [u16; crate::storage::MAX_INDEX_COLS],
    count: usize,
}

impl KeyConstraint {
    fn columns(&self) -> &[u16] {
        &self.columns[..self.count]
    }
}

fn matching_key_constraint(table: &TableDef, columns: &[u16]) -> Option<KeyConstraint> {
    if columns.len() == 1 {
        let column = &table.columns()[columns[0] as usize];
        if column.primary || column.unique {
            return Some(KeyConstraint {
                name: if column.primary {
                    inline_primary_constraint_name(table)
                } else {
                    inline_unique_constraint_name(table, column)
                },
                columns: [columns[0]; crate::storage::MAX_INDEX_COLS],
                count: 1,
            });
        }
    }
    table.uniques().iter().find_map(|key| {
        (key.n_cols == columns.len()
            && columns.iter().all(|column| key.columns().contains(column))
            && key.columns().iter().all(|column| columns.contains(column)))
        .then(|| KeyConstraint {
            name: stack_format!(128, "{}", key.name.as_str()),
            columns: key.columns,
            count: key.n_cols,
        })
    })
}

fn require_parent_key(
    storage: &Storage,
    txid: u32,
    foreign_key: &crate::storage::ForeignKey,
) -> Result<(TableDef, KeyConstraint), SqlError> {
    let parent_slot = storage
        .find_visible(
            foreign_key.parent_schema.as_str(),
            foreign_key.parent.as_str(),
            txid,
        )
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "foreign key \"{}\" has no visible referenced table",
                foreign_key.name.as_str()
            )
        })?;
    let parent = *storage.table_def(parent_slot, txid);
    let key = matching_key_constraint(&parent, foreign_key.parent_cols()).ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "foreign key \"{}\" has no referenced key",
            foreign_key.name.as_str()
        )
    })?;
    Ok((parent, key))
}

fn info_key_column_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "key_column_usage",
        &[
            ("constraint_catalog", ColType::Text),
            ("constraint_schema", ColType::Text),
            ("constraint_name", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("column_name", ColType::Text),
            ("ordinal_position", ColType::Int4),
            ("position_in_unique_constraint", ColType::Int4),
        ],
    );
    const MAX_ROWS: usize = crate::sql::query::MAX_JOIN_TABLES
        * (crate::storage::MAX_COLUMNS
            + crate::storage::MAX_UNIQUES * crate::storage::MAX_INDEX_COLS
            + crate::storage::MAX_FKEYS * crate::storage::MAX_INDEX_COLS);
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0;
    let mut append = |table: &TableDef,
                      name: &str,
                      column: u16,
                      position: usize,
                      parent_position: Option<usize>|
     -> Result<(), SqlError> {
        if count == output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "key_column_usage exceeds static capacity"
            ));
        }
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(table.schema.as_str(), arena)?,
                text(name, arena)?,
                text("postgres", arena)?,
                text(table.schema.as_str(), arena)?,
                text(table.name.as_str(), arena)?,
                text(table.columns()[column as usize].name.as_str(), arena)?,
                Datum::Int4(position as i32),
                parent_position.map_or(Datum::Null, |value| Datum::Int4(value as i32)),
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for (column_index, column) in table.columns().iter().enumerate() {
            if column.primary || column.unique {
                let name = if column.primary {
                    inline_primary_constraint_name(table)
                } else {
                    inline_unique_constraint_name(table, column)
                };
                append(table, name.as_str(), column_index as u16, 1, None)?;
            }
        }
        for key in table.uniques() {
            for (position, &column) in key.columns().iter().enumerate() {
                append(table, key.name.as_str(), column, position + 1, None)?;
            }
        }
        for foreign_key in table.fkeys() {
            let (_, parent_key) = require_parent_key(storage, txid, foreign_key)?;
            for (position, (&column, &parent_column)) in foreign_key
                .columns()
                .iter()
                .zip(foreign_key.parent_cols())
                .enumerate()
            {
                let parent_position = parent_key
                    .columns()
                    .iter()
                    .position(|candidate| *candidate == parent_column)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "foreign key \"{}\" does not map to its referenced key",
                            foreign_key.name.as_str()
                        )
                    })?;
                append(
                    table,
                    foreign_key.name.as_str(),
                    column,
                    position + 1,
                    Some(parent_position + 1),
                )?;
            }
        }
    }
    finish(definition, &output[..count], arena)
}

fn info_constraint_column_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "constraint_column_usage",
        &[
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("column_name", ColType::Text),
            ("constraint_catalog", ColType::Text),
            ("constraint_schema", ColType::Text),
            ("constraint_name", ColType::Text),
        ],
    );
    let mut total = 0usize;
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        total += table
            .columns()
            .iter()
            .filter(|column| column.primary || column.unique)
            .count();
        total += table
            .columns()
            .iter()
            .filter(|column| column.not_null)
            .count();
        total += table.uniques().iter().map(|key| key.n_cols).sum::<usize>();
        total += table
            .fkeys()
            .iter()
            .map(|key| key.n_parent_cols)
            .sum::<usize>();
        for check in table.checks() {
            let expression = crate::sql::parser::parse_expr(check.expression.as_str(), arena)?;
            total += crate::sql::exec::check_referenced_columns(expression, table)?.count_ones()
                as usize;
        }
    }
    let output = arena
        .alloc_slice_with(total, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0;
    let mut append = |referenced_table: &TableDef,
                      constraint_table: &TableDef,
                      referenced_column: u16,
                      constraint_name: &str|
     -> Result<(), SqlError> {
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(referenced_table.schema.as_str(), arena)?,
                text(referenced_table.name.as_str(), arena)?,
                text(
                    referenced_table.columns()[referenced_column as usize]
                        .name
                        .as_str(),
                    arena,
                )?,
                text("postgres", arena)?,
                text(constraint_table.schema.as_str(), arena)?,
                text(constraint_name, arena)?,
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for (column_index, column) in table.columns().iter().enumerate() {
            if column.primary || column.unique {
                let name = if column.primary {
                    inline_primary_constraint_name(table)
                } else {
                    inline_unique_constraint_name(table, column)
                };
                append(table, table, column_index as u16, name.as_str())?;
            }
            if column.not_null {
                let name = not_null_constraint_name(table, column);
                append(table, table, column_index as u16, name.as_str())?;
            }
        }
        for key in table.uniques() {
            for &column in key.columns() {
                append(table, table, column, key.name.as_str())?;
            }
        }
        for check in table.checks() {
            let expression = crate::sql::parser::parse_expr(check.expression.as_str(), arena)?;
            let columns = crate::sql::exec::check_referenced_columns(expression, table)?;
            for (index, _) in table.columns().iter().enumerate() {
                if columns & (1u64 << index) != 0 {
                    append(table, table, index as u16, check.name.as_str())?;
                }
            }
        }
        for foreign_key in table.fkeys() {
            let (parent, _) = require_parent_key(storage, txid, foreign_key)?;
            for &column in foreign_key.parent_cols() {
                append(&parent, table, column, foreign_key.name.as_str())?;
            }
        }
    }
    debug_assert_eq!(count, output.len());
    finish(definition, output, arena)
}

fn info_table_privileges<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    info_relation_privileges(storage, txid, arena, "table_privileges", true)
}

fn info_role_table_grants<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    info_relation_privileges(storage, txid, arena, "role_table_grants", false)
}

fn info_sequences<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "sequences",
        &[
            ("sequence_catalog", ColType::Text),
            ("sequence_schema", ColType::Text),
            ("sequence_name", ColType::Text),
            ("data_type", ColType::Text),
            ("numeric_precision", ColType::Int4),
            ("numeric_precision_radix", ColType::Int4),
            ("numeric_scale", ColType::Int4),
            ("start_value", ColType::Text),
            ("minimum_value", ColType::Text),
            ("maximum_value", ColType::Text),
            ("increment", ColType::Text),
            ("cycle_option", ColType::Text),
        ],
    );
    let mut output: [&[Datum]; 256] = [&[]; 256];
    let mut count = 0usize;
    for slot in 0..storage.sequence_count() {
        let sequence = storage.sequence_for(slot, txid);
        if !sequence.visible_to(txid) {
            continue;
        }
        if count == output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "information_schema.sequences exceeds static capacity"
            ));
        }
        let precision = match sequence.data_type {
            crate::storage::SeqType::Smallint => 16,
            crate::storage::SeqType::Integer => 32,
            crate::storage::SeqType::Bigint => 64,
        };
        let start = stack_format!(32, "{}", sequence.start_value);
        let minimum = stack_format!(32, "{}", sequence.min_value);
        let maximum = stack_format!(32, "{}", sequence.max_value);
        let increment = stack_format!(32, "{}", sequence.increment);
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(sequence.schema.as_str(), arena)?,
                text(sequence.name.as_str(), arena)?,
                text(sequence.data_type.sql_name(), arena)?,
                Datum::Int4(precision),
                Datum::Int4(2),
                Datum::Int4(0),
                text(start.as_str(), arena)?,
                text(minimum.as_str(), arena)?,
                text(maximum.as_str(), arena)?,
                text(increment.as_str(), arena)?,
                text(if sequence.cycle { "YES" } else { "NO" }, arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

fn info_usage_privileges<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "usage_privileges",
        &[
            ("grantor", ColType::Text),
            ("grantee", ColType::Text),
            ("object_catalog", ColType::Text),
            ("object_schema", ColType::Text),
            ("object_name", ColType::Text),
            ("object_type", ColType::Text),
            ("privilege_type", ColType::Text),
            ("is_grantable", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = 512 + crate::storage::MAX_ACL_ENTRIES;
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0usize;
    let mut append = |object: crate::storage::AccessObject,
                      object_type: &str,
                      grantor: u16,
                      grantee: u16,
                      grantable: bool|
     -> Result<(), SqlError> {
        if !storage.role_is_enabled(grantor, txid) && !storage.role_is_enabled(grantee, txid) {
            return Ok(());
        }
        if count == output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "usage_privileges exceeds static capacity"
            ));
        }
        let (schema, name) = storage.access_object_name_to(object, txid);
        let grantor_name = storage.role_name(grantor as usize, txid);
        let grantee_name = if grantee == crate::storage::PUBLIC_ROLE {
            SqlName::parse("PUBLIC").expect("PUBLIC fits a SQL name")
        } else {
            storage.role_name(grantee as usize, txid)
        };
        output[count] = row(
            &[
                text(grantor_name.as_str(), arena)?,
                text(grantee_name.as_str(), arena)?,
                text("postgres", arena)?,
                text(schema.as_str(), arena)?,
                text(name.as_str(), arena)?,
                text(object_type, arena)?,
                text("USAGE", arena)?,
                text(if grantable { "YES" } else { "NO" }, arena)?,
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };
    let mut append_object = |object: crate::storage::AccessObject,
                             object_type: &str,
                             public_default: bool|
     -> Result<(), SqlError> {
        let owner = storage.object_owner(object, txid) as u16;
        append(object, object_type, owner, owner, true)?;
        let public_defined = storage.acl_entries().any(|(slot, entry)| {
            entry.object == object
                && storage.acl_identity(slot, txid).0 == crate::storage::PUBLIC_ROLE
        });
        if public_default && !public_defined {
            append(
                object,
                object_type,
                owner,
                crate::storage::PUBLIC_ROLE,
                false,
            )?;
        }
        for (slot, entry) in storage.acl_entries() {
            if entry.object != object {
                continue;
            }
            let (privileges, options) = storage.acl_state(slot, txid);
            if !privileges.contains(crate::storage::PrivilegeSet::USAGE) {
                continue;
            }
            let (grantee, grantor) = storage.acl_identity(slot, txid);
            if grantee == owner && grantor == owner {
                continue;
            }
            append(
                object,
                object_type,
                grantor,
                grantee,
                options.contains(crate::storage::PrivilegeSet::USAGE),
            )?;
        }
        Ok(())
    };
    for slot in 0..storage.sequence_count() {
        if !storage.sequence_for(slot, txid).visible_to(txid) {
            continue;
        }
        append_object(
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::Sequence,
                slot: slot as u16,
            },
            "SEQUENCE",
            false,
        )?;
    }
    for slot in 0..storage.domain_count() {
        if !storage.domain(slot).visible_to(txid) {
            continue;
        }
        append_object(
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::Domain,
                slot: slot as u16,
            },
            "DOMAIN",
            true,
        )?;
    }
    finish(definition, &output[..count], arena)
}

fn info_relation_privileges<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    relation_name: &str,
    include_public: bool,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        relation_name,
        &[
            ("grantor", ColType::Text),
            ("grantee", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("privilege_type", ColType::Text),
            ("is_grantable", ColType::Text),
            ("with_hierarchy", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = 8 * (512 + crate::storage::MAX_ACL_ENTRIES);
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0usize;
    let mut append = |object: crate::storage::AccessObject,
                      grantor: u16,
                      grantee: u16,
                      privilege_mask: crate::storage::PrivilegeSet,
                      grant_options: crate::storage::PrivilegeSet|
     -> Result<(), SqlError> {
        if !include_public && grantee == crate::storage::PUBLIC_ROLE {
            return Ok(());
        }
        if !storage.role_is_enabled(grantor, txid) && !storage.role_is_enabled(grantee, txid) {
            return Ok(());
        }
        let (schema, name) = storage.access_object_name_to(object, txid);
        let privilege_names = [
            (crate::storage::PrivilegeSet::SELECT, "SELECT"),
            (crate::storage::PrivilegeSet::INSERT, "INSERT"),
            (crate::storage::PrivilegeSet::UPDATE, "UPDATE"),
            (crate::storage::PrivilegeSet::DELETE, "DELETE"),
            (crate::storage::PrivilegeSet::TRUNCATE, "TRUNCATE"),
            (crate::storage::PrivilegeSet::REFERENCES, "REFERENCES"),
            (crate::storage::PrivilegeSet::TRIGGER, "TRIGGER"),
        ];
        for (privilege, privilege_name) in privilege_names {
            if !privilege_mask.contains(privilege) {
                continue;
            }
            if count == output.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table_privileges exceeds static capacity"
                ));
            }
            let grantor_name = storage.role_name(grantor as usize, txid);
            let grantee_name = if grantee == crate::storage::PUBLIC_ROLE {
                SqlName::parse("PUBLIC").expect("PUBLIC fits a SQL name")
            } else {
                storage.role_name(grantee as usize, txid)
            };
            output[count] = row(
                &[
                    text(grantor_name.as_str(), arena)?,
                    text(grantee_name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(schema.as_str(), arena)?,
                    text(name.as_str(), arena)?,
                    text(privilege_name, arena)?,
                    text(
                        if grant_options.contains(privilege) {
                            "YES"
                        } else {
                            "NO"
                        },
                        arena,
                    )?,
                    text(
                        if privilege == crate::storage::PrivilegeSet::SELECT {
                            "YES"
                        } else {
                            "NO"
                        },
                        arena,
                    )?,
                ],
                arena,
            )?;
            count += 1;
        }
        Ok(())
    };
    let mut append_object = |object: crate::storage::AccessObject| -> Result<(), SqlError> {
        let owner = storage.object_owner(object, txid) as u16;
        append(
            object,
            owner,
            owner,
            crate::storage::PrivilegeSet::TABLE_ALL,
            crate::storage::PrivilegeSet::TABLE_ALL,
        )?;
        for (slot, entry) in storage.acl_entries() {
            if entry.object != object {
                continue;
            }
            let (privileges, options) = storage.acl_state(slot, txid);
            if privileges.0 == 0 {
                continue;
            }
            let (grantee, grantor) = storage.acl_identity(slot, txid);
            if grantee == owner && grantor == owner {
                continue;
            }
            append(object, grantor, grantee, privileges, options)?;
        }
        Ok(())
    };
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        let (class, object_slot) =
            match storage.matview_slot(table.schema.as_str(), table.name.as_str(), txid) {
                Some(matview) => (crate::storage::AccessClass::MaterializedView, matview),
                None => (crate::storage::AccessClass::Table, slot),
            };
        append_object(crate::storage::AccessObject {
            class,
            slot: object_slot as u16,
        })?;
    }
    for (slot, _) in storage.views_visible_to(txid) {
        append_object(crate::storage::AccessObject {
            class: crate::storage::AccessClass::View,
            slot: slot as u16,
        })?;
    }
    finish(definition, &output[..count], arena)
}

fn column_privilege_count(
    storage: &Storage,
    txid: u32,
    object: crate::storage::AccessObject,
    include_public: bool,
) -> usize {
    let visible_privileges = [
        crate::storage::PrivilegeSet::SELECT,
        crate::storage::PrivilegeSet::INSERT,
        crate::storage::PrivilegeSet::UPDATE,
        crate::storage::PrivilegeSet::REFERENCES,
    ];
    let owner = storage.object_owner(object, txid) as u16;
    let mut count = storage.role_is_enabled(owner, txid) as usize * visible_privileges.len();
    for (slot, entry) in storage.acl_entries() {
        if entry.object != object {
            continue;
        }
        let (grantee, grantor) = storage.acl_identity(slot, txid);
        if (!include_public && grantee == crate::storage::PUBLIC_ROLE)
            || (!storage.role_is_enabled(grantor, txid) && !storage.role_is_enabled(grantee, txid))
            || (grantee == owner && grantor == owner)
        {
            continue;
        }
        let (privileges, _) = storage.acl_state(slot, txid);
        count += visible_privileges
            .iter()
            .filter(|privilege| privileges.contains(**privilege))
            .count();
    }
    count
}

fn info_column_privileges<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    include_public: bool,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        if include_public {
            "column_privileges"
        } else {
            "role_column_grants"
        },
        &[
            ("grantor", ColType::Text),
            ("grantee", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("column_name", ColType::Text),
            ("privilege_type", ColType::Text),
            ("is_grantable", ColType::Text),
        ],
    );
    let mut output_count = 0usize;
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        let object = crate::storage::AccessObject {
            class: match storage.matview_slot(table.schema.as_str(), table.name.as_str(), txid) {
                Some(_) => crate::storage::AccessClass::MaterializedView,
                None => crate::storage::AccessClass::Table,
            },
            slot: storage
                .matview_slot(table.schema.as_str(), table.name.as_str(), txid)
                .unwrap_or(slot) as u16,
        };
        output_count = output_count
            .checked_add(
                table
                    .columns()
                    .len()
                    .checked_mul(column_privilege_count(
                        storage,
                        txid,
                        object,
                        include_public,
                    ))
                    .ok_or_else(|| {
                        catalog_capacity_exceeded("information_schema.column_privileges")
                    })?,
            )
            .ok_or_else(|| catalog_capacity_exceeded("information_schema.column_privileges"))?;
    }
    for (view_slot, view) in storage.views_visible_to(txid) {
        let mut columns = [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
        let column_count = describe_view(storage, txid, view, arena, &mut columns)?;
        output_count = output_count
            .checked_add(
                column_count
                    .checked_mul(column_privilege_count(
                        storage,
                        txid,
                        crate::storage::AccessObject {
                            class: crate::storage::AccessClass::View,
                            slot: view_slot as u16,
                        },
                        include_public,
                    ))
                    .ok_or_else(|| {
                        catalog_capacity_exceeded("information_schema.column_privileges")
                    })?,
            )
            .ok_or_else(|| catalog_capacity_exceeded("information_schema.column_privileges"))?;
    }
    let output = arena
        .alloc_slice_with(output_count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0usize;
    let mut append = |schema: &str,
                      table: &str,
                      column: &str,
                      grantor: u16,
                      grantee: u16,
                      privileges: crate::storage::PrivilegeSet,
                      grant_options: crate::storage::PrivilegeSet|
     -> Result<(), SqlError> {
        if (!include_public && grantee == crate::storage::PUBLIC_ROLE)
            || (!storage.role_is_enabled(grantor, txid) && !storage.role_is_enabled(grantee, txid))
        {
            return Ok(());
        }
        let grantor_name = storage.role_name(grantor as usize, txid);
        let grantee_name = if grantee == crate::storage::PUBLIC_ROLE {
            SqlName::parse("PUBLIC").expect("PUBLIC fits a SQL name")
        } else {
            storage.role_name(grantee as usize, txid)
        };
        for (privilege, privilege_name) in [
            (crate::storage::PrivilegeSet::SELECT, "SELECT"),
            (crate::storage::PrivilegeSet::INSERT, "INSERT"),
            (crate::storage::PrivilegeSet::UPDATE, "UPDATE"),
            (crate::storage::PrivilegeSet::REFERENCES, "REFERENCES"),
        ] {
            if !privileges.contains(privilege) {
                continue;
            }
            debug_assert!(count < output.len());
            output[count] = row(
                &[
                    text(grantor_name.as_str(), arena)?,
                    text(grantee_name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(schema, arena)?,
                    text(table, arena)?,
                    text(column, arena)?,
                    text(privilege_name, arena)?,
                    text(
                        if grant_options.contains(privilege) {
                            "YES"
                        } else {
                            "NO"
                        },
                        arena,
                    )?,
                ],
                arena,
            )?;
            count += 1;
        }
        Ok(())
    };
    let mut append_relation = |object: crate::storage::AccessObject,
                               schema: &str,
                               table: &str,
                               columns: &[ColumnMeta]|
     -> Result<(), SqlError> {
        let owner = storage.object_owner(object, txid) as u16;
        for column in columns {
            append(
                schema,
                table,
                column.name.as_str(),
                owner,
                owner,
                crate::storage::PrivilegeSet::SELECT
                    .union(crate::storage::PrivilegeSet::INSERT)
                    .union(crate::storage::PrivilegeSet::UPDATE)
                    .union(crate::storage::PrivilegeSet::REFERENCES),
                crate::storage::PrivilegeSet::SELECT
                    .union(crate::storage::PrivilegeSet::INSERT)
                    .union(crate::storage::PrivilegeSet::UPDATE)
                    .union(crate::storage::PrivilegeSet::REFERENCES),
            )?;
            for (slot, entry) in storage.acl_entries() {
                if entry.object != object {
                    continue;
                }
                let (grantee, grantor) = storage.acl_identity(slot, txid);
                if grantee == owner && grantor == owner {
                    continue;
                }
                let (privileges, grant_options) = storage.acl_state(slot, txid);
                append(
                    schema,
                    table,
                    column.name.as_str(),
                    grantor,
                    grantee,
                    privileges,
                    grant_options,
                )?;
            }
        }
        Ok(())
    };
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        let object = match storage.matview_slot(table.schema.as_str(), table.name.as_str(), txid) {
            Some(matview_slot) => crate::storage::AccessObject {
                class: crate::storage::AccessClass::MaterializedView,
                slot: matview_slot as u16,
            },
            None => crate::storage::AccessObject {
                class: crate::storage::AccessClass::Table,
                slot: slot as u16,
            },
        };
        append_relation(
            object,
            table.schema.as_str(),
            table.name.as_str(),
            table.columns(),
        )?;
    }
    for (slot, view) in storage.views_visible_to(txid) {
        let mut descriptions = [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
        let description_count = describe_view(storage, txid, view, arena, &mut descriptions)?;
        let mut columns = [ColumnMeta::EMPTY; super::exec::MAX_PROJ];
        for (index, description) in descriptions[..description_count].iter().enumerate() {
            let (ctype, user_type) = view_column_catalog_type(storage, txid, description.type_oid)?;
            columns[index] = ColumnMeta {
                name: SqlName::parse(description.name)?,
                ctype,
                type_mod: description.type_mod,
                not_null: false,
                unique: false,
                primary: false,
                auto_increment: false,
                default: crate::storage::ColumnDefault::NONE,
                is_identity: false,
                identity_always: false,
                auto_increment_step: 1,
                user_type,
            };
        }
        append_relation(
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::View,
                slot: slot as u16,
            },
            view.schema.as_str(),
            view.name.as_str(),
            &columns[..description_count],
        )?;
    }
    debug_assert_eq!(count, output.len());
    finish(definition, output, arena)
}

fn fk_action_name(action: crate::storage::FkAction) -> &'static str {
    match action {
        crate::storage::FkAction::NoAction => "NO ACTION",
        crate::storage::FkAction::Restrict => "RESTRICT",
        crate::storage::FkAction::Cascade => "CASCADE",
        crate::storage::FkAction::SetNull => "SET NULL",
        crate::storage::FkAction::SetDefault => "SET DEFAULT",
    }
}

fn info_referential_constraints<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "referential_constraints",
        &[
            ("constraint_catalog", ColType::Text),
            ("constraint_schema", ColType::Text),
            ("constraint_name", ColType::Text),
            ("unique_constraint_catalog", ColType::Text),
            ("unique_constraint_schema", ColType::Text),
            ("unique_constraint_name", ColType::Text),
            ("match_option", ColType::Text),
            ("update_rule", ColType::Text),
            ("delete_rule", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = crate::sql::query::MAX_JOIN_TABLES * crate::storage::MAX_FKEYS;
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0;
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for foreign_key in table.fkeys() {
            if count == output.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "referential_constraints exceeds static capacity"
                ));
            }
            let (parent, parent_key) = require_parent_key(storage, txid, foreign_key)?;
            output[count] = row(
                &[
                    text("postgres", arena)?,
                    text(table.schema.as_str(), arena)?,
                    text(foreign_key.name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(parent.schema.as_str(), arena)?,
                    text(parent_key.name.as_str(), arena)?,
                    text("NONE", arena)?,
                    text(fk_action_name(foreign_key.on_update), arena)?,
                    text(fk_action_name(foreign_key.on_delete), arena)?,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &output[..count], arena)
}

fn info_domains<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "domains",
        &[
            ("domain_catalog", ColType::Text),
            ("domain_schema", ColType::Text),
            ("domain_name", ColType::Text),
            ("data_type", ColType::Text),
            ("character_maximum_length", ColType::Int4),
            ("character_octet_length", ColType::Int4),
            ("character_set_catalog", ColType::Text),
            ("character_set_schema", ColType::Text),
            ("character_set_name", ColType::Text),
            ("collation_catalog", ColType::Text),
            ("collation_schema", ColType::Text),
            ("collation_name", ColType::Text),
            ("numeric_precision", ColType::Int4),
            ("numeric_precision_radix", ColType::Int4),
            ("numeric_scale", ColType::Int4),
            ("datetime_precision", ColType::Int4),
            ("interval_type", ColType::Text),
            ("interval_precision", ColType::Int4),
            ("domain_default", ColType::Text),
            ("udt_catalog", ColType::Text),
            ("udt_schema", ColType::Text),
            ("udt_name", ColType::Text),
            ("scope_catalog", ColType::Text),
            ("scope_schema", ColType::Text),
            ("scope_name", ColType::Text),
            ("maximum_cardinality", ColType::Int4),
            ("dtd_identifier", ColType::Text),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_DOMAINS] = [&[]; crate::storage::MAX_DOMAINS];
    let mut count = 0;
    for slot in 0..storage.domain_count() {
        let domain = storage.domain_for(slot, txid);
        if !domain.visible_to(txid) {
            continue;
        }
        let type_mod = TypeMod::decode(domain.base, domain.base_type_mod);
        let (character_length, numeric_precision, numeric_radix, numeric_scale, datetime_precision) =
            information_schema_scalar_metadata(domain.base, type_mod);
        let (data_type, udt_schema, udt_name) = match domain.base_domain {
            Some(parent) => (
                "USER-DEFINED",
                parent.schema,
                StackStr::<64>::from_str(parent.name.as_str()),
            ),
            None => (
                domain.base.name(),
                SqlName::parse("pg_catalog").expect("catalog schema fits"),
                StackStr::<64>::from_str(domain.base.catalog_name()),
            ),
        };
        let default = match domain.default_expr {
            Some(value) => text(value.as_str(), arena)?,
            None => Datum::Null,
        };
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(domain.schema.as_str(), arena)?,
                text(domain.name.as_str(), arena)?,
                text(data_type, arena)?,
                character_length.map_or(Datum::Null, Datum::Int4),
                character_length.map_or(Datum::Null, Datum::Int4),
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                numeric_precision.map_or(Datum::Null, Datum::Int4),
                numeric_radix.map_or(Datum::Null, Datum::Int4),
                numeric_scale.map_or(Datum::Null, Datum::Int4),
                datetime_precision.map_or(Datum::Null, Datum::Int4),
                Datum::Null,
                Datum::Null,
                default,
                text("postgres", arena)?,
                text(udt_schema.as_str(), arena)?,
                text(udt_name.as_str(), arena)?,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                text("1", arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

fn info_domain_constraints<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "domain_constraints",
        &[
            ("constraint_catalog", ColType::Text),
            ("constraint_schema", ColType::Text),
            ("constraint_name", ColType::Text),
            ("domain_catalog", ColType::Text),
            ("domain_schema", ColType::Text),
            ("domain_name", ColType::Text),
            ("is_deferrable", ColType::Text),
            ("initially_deferred", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = crate::storage::MAX_DOMAINS * (crate::storage::MAX_DOMAIN_CHECKS + 1);
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0;
    for slot in 0..storage.domain_count() {
        let domain = storage.domain_for(slot, txid);
        if !domain.visible_to(txid) {
            continue;
        }
        for check in domain.checks() {
            output[count] = row(
                &[
                    text("postgres", arena)?,
                    text(domain.schema.as_str(), arena)?,
                    text(check.name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(domain.schema.as_str(), arena)?,
                    text(domain.name.as_str(), arena)?,
                    text("NO", arena)?,
                    text("NO", arena)?,
                ],
                arena,
            )?;
            count += 1;
        }
        if domain.not_null {
            let name = stack_format!(128, "{}_not_null", domain.name.as_str());
            output[count] = row(
                &[
                    text("postgres", arena)?,
                    text(domain.schema.as_str(), arena)?,
                    text(name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(domain.schema.as_str(), arena)?,
                    text(domain.name.as_str(), arena)?,
                    text("NO", arena)?,
                    text("NO", arena)?,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &output[..count], arena)
}

fn info_check_constraints<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "check_constraints",
        &[
            ("constraint_catalog", ColType::Text),
            ("constraint_schema", ColType::Text),
            ("constraint_name", ColType::Text),
            ("check_clause", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = crate::sql::query::MAX_JOIN_TABLES
        * (crate::storage::MAX_CHECKS + MAX_COLUMNS)
        + crate::storage::MAX_DOMAINS * (crate::storage::MAX_DOMAIN_CHECKS + 1);
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0;
    let mut append = |schema: &str, name: &str, clause: &str| -> Result<(), SqlError> {
        if count == output.len() {
            return Err(catalog_capacity_exceeded(
                "information_schema.check_constraints",
            ));
        }
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(schema, arena)?,
                text(name, arena)?,
                text(clause, arena)?,
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for check in table.checks() {
            let clause = stack_format!(1024, "({})", check.expression.as_str());
            append(table.schema.as_str(), check.name.as_str(), clause.as_str())?;
        }
        for column in table.columns() {
            if column.not_null {
                let name = not_null_constraint_name(table, column);
                let clause = stack_format!(256, "{} IS NOT NULL", column.name.as_str());
                append(table.schema.as_str(), name.as_str(), clause.as_str())?;
            }
        }
    }
    for slot in 0..storage.domain_count() {
        let domain = storage.domain_for(slot, txid);
        if !domain.visible_to(txid) {
            continue;
        }
        for check in domain.checks() {
            let clause = stack_format!(1024, "({})", check.expression.as_str());
            append(domain.schema.as_str(), check.name.as_str(), clause.as_str())?;
        }
        if domain.not_null {
            let name = stack_format!(128, "{}_not_null", domain.name.as_str());
            append(domain.schema.as_str(), name.as_str(), "VALUE IS NOT NULL")?;
        }
    }
    finish(definition, &output[..count], arena)
}

#[derive(Clone, Copy)]
enum InformationSchemaTypeUsage {
    Domain,
    UnderlyingType,
}

fn info_column_domain_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    info_column_type_usage(storage, txid, arena, InformationSchemaTypeUsage::Domain)
}

fn info_column_udt_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    info_column_type_usage(
        storage,
        txid,
        arena,
        InformationSchemaTypeUsage::UnderlyingType,
    )
}

fn info_column_type_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    usage: InformationSchemaTypeUsage,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        match usage {
            InformationSchemaTypeUsage::Domain => "column_domain_usage",
            InformationSchemaTypeUsage::UnderlyingType => "column_udt_usage",
        },
        match usage {
            InformationSchemaTypeUsage::Domain => &[
                ("domain_catalog", ColType::Text),
                ("domain_schema", ColType::Text),
                ("domain_name", ColType::Text),
                ("table_catalog", ColType::Text),
                ("table_schema", ColType::Text),
                ("table_name", ColType::Text),
                ("column_name", ColType::Text),
            ],
            InformationSchemaTypeUsage::UnderlyingType => &[
                ("udt_catalog", ColType::Text),
                ("udt_schema", ColType::Text),
                ("udt_name", ColType::Text),
                ("table_catalog", ColType::Text),
                ("table_schema", ColType::Text),
                ("table_name", ColType::Text),
                ("column_name", ColType::Text),
            ],
        },
    );
    let table_capacity = storage
        .table_count()
        .checked_mul(MAX_COLUMNS)
        .ok_or_else(|| catalog_capacity_exceeded("information_schema column type usage"))?;
    let view_capacity = storage
        .view_count()
        .checked_mul(super::exec::MAX_PROJ)
        .ok_or_else(|| catalog_capacity_exceeded("information_schema column type usage"))?;
    let output = arena
        .alloc_slice_with(
            table_capacity
                .checked_add(view_capacity)
                .ok_or_else(|| catalog_capacity_exceeded("information_schema column type usage"))?,
            |_| &[] as &[Datum],
        )
        .map_err(|_| arena_full())?;
    let mut count = 0;
    let mut append =
        |schema: &str, table: &str, column: &str, metadata: &ColumnMeta| -> Result<(), SqlError> {
            let declared_oid = catalog_column_type_oid(storage, metadata, txid)?;
            let identity =
                information_schema_usage_type(storage, txid, metadata, declared_oid, usage)?;
            let Some((type_schema, type_name)) = identity else {
                return Ok(());
            };
            debug_assert!(count < output.len());
            output[count] = row(
                &[
                    text("postgres", arena)?,
                    text(type_schema.as_str(), arena)?,
                    text(type_name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(schema, arena)?,
                    text(table, arena)?,
                    text(column, arena)?,
                ],
                arena,
            )?;
            count += 1;
            Ok(())
        };
    for slot in 0..storage.table_count() {
        if !storage.table(slot).visible_to(txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for column in table.columns() {
            append(
                table.schema.as_str(),
                table.name.as_str(),
                column.name.as_str(),
                column,
            )?;
        }
    }
    for (_, view) in storage.views_visible_to(txid) {
        let mut columns = [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
        let column_count = describe_view(storage, txid, view, arena, &mut columns)?;
        for column in &columns[..column_count] {
            let (ctype, user_type) = view_column_catalog_type(storage, txid, column.type_oid)?;
            let metadata = ColumnMeta {
                name: SqlName::EMPTY,
                ctype,
                type_mod: column.type_mod,
                not_null: false,
                unique: false,
                primary: false,
                auto_increment: false,
                default: crate::storage::ColumnDefault::NONE,
                is_identity: false,
                identity_always: false,
                auto_increment_step: 1,
                user_type,
            };
            append(
                view.schema.as_str(),
                view.name.as_str(),
                column.name,
                &metadata,
            )?;
        }
    }
    finish(definition, &output[..count], arena)
}

fn information_schema_usage_type(
    storage: &Storage,
    txid: u32,
    column: &ColumnMeta,
    declared_oid: i32,
    usage: InformationSchemaTypeUsage,
) -> Result<Option<(SqlName, StackStr<64>)>, SqlError> {
    use crate::sql::types::oid;
    let is_domain = (oid::FIRST_DOMAIN..oid::FIRST_DOMAIN + crate::storage::MAX_DOMAINS as i32)
        .contains(&declared_oid);
    match usage {
        InformationSchemaTypeUsage::Domain => Ok(is_domain.then(|| {
            let domain = storage.domain_for((declared_oid - oid::FIRST_DOMAIN) as usize, txid);
            (domain.schema, StackStr::from_str(domain.name.as_str()))
        })),
        InformationSchemaTypeUsage::UnderlyingType if is_domain => {
            let domain = storage.domain_for((declared_oid - oid::FIRST_DOMAIN) as usize, txid);
            Ok(Some(match domain.base_domain {
                Some(parent) => (parent.schema, StackStr::from_str(parent.name.as_str())),
                None => (
                    SqlName::parse("pg_catalog").expect("catalog schema fits"),
                    StackStr::from_str(domain.base.catalog_name()),
                ),
            }))
        }
        InformationSchemaTypeUsage::UnderlyingType => Ok(Some(match column.user_type {
            Some(identity) => {
                let mut name = StackStr::<64>::new();
                if matches!(column.ctype, ColType::Array(_)) {
                    use core::fmt::Write as _;
                    let _ = write!(name, "_{}", identity.name.as_str());
                } else {
                    name = StackStr::from_str(identity.name.as_str());
                }
                (identity.schema, name)
            }
            None => (
                SqlName::parse("pg_catalog").expect("catalog schema fits"),
                StackStr::from_str(column.ctype.catalog_name()),
            ),
        })),
    }
}

fn info_domain_udt_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "domain_udt_usage",
        &[
            ("udt_catalog", ColType::Text),
            ("udt_schema", ColType::Text),
            ("udt_name", ColType::Text),
            ("domain_catalog", ColType::Text),
            ("domain_schema", ColType::Text),
            ("domain_name", ColType::Text),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_DOMAINS] = [&[]; crate::storage::MAX_DOMAINS];
    let mut count = 0;
    for slot in 0..storage.domain_count() {
        let domain = storage.domain_for(slot, txid);
        if !domain.visible_to(txid) {
            continue;
        }
        let (udt_schema, udt_name) = match domain.base_domain {
            Some(parent) => (
                parent.schema,
                StackStr::<64>::from_str(parent.name.as_str()),
            ),
            None => (
                SqlName::parse("pg_catalog").expect("catalog schema fits"),
                StackStr::<64>::from_str(domain.base.catalog_name()),
            ),
        };
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(udt_schema.as_str(), arena)?,
                text(udt_name.as_str(), arena)?,
                text("postgres", arena)?,
                text(domain.schema.as_str(), arena)?,
                text(domain.name.as_str(), arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

type InformationSchemaScalarMetadata = (
    Option<i32>,
    Option<i32>,
    Option<i32>,
    Option<i32>,
    Option<i32>,
);

fn information_schema_scalar_metadata(
    ctype: ColType,
    type_mod: TypeMod,
) -> InformationSchemaScalarMetadata {
    match type_mod {
        TypeMod::Length(length) if matches!(ctype, ColType::Varchar | ColType::Bpchar) => {
            (Some(length as i32), None, None, None, None)
        }
        TypeMod::NumericPS { precision, scale } => (
            None,
            Some(precision as i32),
            Some(10),
            Some(scale as i32),
            None,
        ),
        TypeMod::TemporalPrecision(precision) => (None, None, None, None, Some(precision as i32)),
        TypeMod::IntervalMod { precision, .. } => {
            (None, None, None, None, precision.map(i32::from))
        }
        _ => match ctype {
            ColType::Int2 => (None, Some(16), Some(2), Some(0), None),
            ColType::Int4 => (None, Some(32), Some(2), Some(0), None),
            ColType::Int8 => (None, Some(64), Some(2), Some(0), None),
            ColType::Float4 => (None, Some(24), Some(2), None, None),
            ColType::Float8 => (None, Some(53), Some(2), None, None),
            _ => (None, None, None, None, None),
        },
    }
}

fn info_schemata<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "schemata",
        &[
            ("catalog_name", ColType::Text),
            ("schema_name", ColType::Text),
            ("schema_owner", ColType::Text),
            ("default_character_set_catalog", ColType::Text),
            ("default_character_set_schema", ColType::Text),
            ("default_character_set_name", ColType::Text),
            ("sql_path", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 2 + crate::storage::MAX_SCHEMAS] =
        [&[]; 2 + crate::storage::MAX_SCHEMAS];
    let mut n = 0;
    for (slot, schema) in storage.visible_schemas(txid) {
        out[n] = row(
            &[
                text("postgres", arena)?,
                text(
                    arena
                        .alloc_str(schema.name.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text(
                    catalog_owner_name(
                        storage,
                        catalog_owner(
                            storage,
                            crate::storage::AccessObject {
                                class: crate::storage::AccessClass::Schema,
                                slot: slot as u16,
                            },
                            txid,
                        ),
                        txid,
                    )
                    .as_str(),
                    arena,
                )?,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    out[n] = row(
        &[
            text("postgres", arena)?,
            text("pg_catalog", arena)?,
            text("postgres", arena)?,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
        ],
        arena,
    )?;
    n += 1;
    out[n] = row(
        &[
            text("postgres", arena)?,
            text("information_schema", arena)?,
            text("postgres", arena)?,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
        ],
        arena,
    )?;
    n += 1;
    finish(def, &out[..n], arena)
}

fn info_collations<'a>(arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "collations",
        &[
            ("collation_catalog", ColType::Text),
            ("collation_schema", ColType::Text),
            ("collation_name", ColType::Text),
            ("pad_attribute", ColType::Text),
        ],
    );
    let names = ["default", "C", "POSIX", "ucs_basic"];
    let mut output: [&[Datum]; 4] = [&[]; 4];
    for (index, name) in names.iter().enumerate() {
        output[index] = row(
            &[
                text("postgres", arena)?,
                text("pg_catalog", arena)?,
                text(name, arena)?,
                text("NO PAD", arena)?,
            ],
            arena,
        )?;
    }
    finish(definition, &output, arena)
}

fn info_collation_character_set_applicability<'a>(
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "collation_character_set_applicability",
        &[
            ("collation_catalog", ColType::Text),
            ("collation_schema", ColType::Text),
            ("collation_name", ColType::Text),
            ("character_set_catalog", ColType::Text),
            ("character_set_schema", ColType::Text),
            ("character_set_name", ColType::Text),
        ],
    );
    let names = ["default", "C", "POSIX", "ucs_basic"];
    let mut output: [&[Datum]; 4] = [&[]; 4];
    for (index, name) in names.iter().enumerate() {
        output[index] = row(
            &[
                text("postgres", arena)?,
                text("pg_catalog", arena)?,
                text(name, arena)?,
                Datum::Null,
                Datum::Null,
                text("UTF8", arena)?,
            ],
            arena,
        )?;
    }
    finish(definition, &output, arena)
}

fn info_enabled_roles<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of("enabled_roles", &[("role_name", ColType::Text)]);
    let mut output: [&[Datum]; crate::storage::MAX_ROLES] = [&[]; crate::storage::MAX_ROLES];
    let mut count = 0;
    for slot in 0..storage.role_count() {
        if !storage.role(slot).visible_to(txid) || !storage.role_is_enabled(slot as u16, txid) {
            continue;
        }
        output[count] = row(
            &[text(storage.role_name(slot, txid).as_str(), arena)?],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

fn info_applicable_roles<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    administrators_only: bool,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        if administrators_only {
            "administrable_role_authorizations"
        } else {
            "applicable_roles"
        },
        &[
            ("grantee", ColType::Text),
            ("role_name", ColType::Text),
            ("is_grantable", ColType::Text),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_ROLE_MEMBERSHIPS + 1] =
        [&[]; crate::storage::MAX_ROLE_MEMBERSHIPS + 1];
    let mut count = 0;
    let mut append = |grantee: &str, role: &str, admin: bool| -> Result<(), SqlError> {
        if administrators_only && !admin {
            return Ok(());
        }
        if count == output.len() {
            return Err(catalog_capacity_exceeded(
                "information_schema.applicable_roles",
            ));
        }
        output[count] = row(
            &[
                text(grantee, arena)?,
                text(role, arena)?,
                text(if admin { "YES" } else { "NO" }, arena)?,
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };
    for slot in 0..storage.role_membership_count() {
        let membership = storage.role_membership(slot);
        if !membership.visible_to(txid) || !storage.role_is_enabled(membership.member, txid) {
            continue;
        }
        append(
            storage.role_name(membership.member as usize, txid).as_str(),
            storage.role_name(membership.role as usize, txid).as_str(),
            membership.options_to(txid).admin,
        )?;
    }
    if storage.role_count() > 0 && storage.role_is_enabled(0, txid) {
        append(
            storage.role_name(0, txid).as_str(),
            "pg_database_owner",
            false,
        )?;
    }
    finish(definition, &output[..count], arena)
}

fn arena_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "catalog relation exceeds the statement arena"
    )
}

fn alloc_rendered<'a, const N: usize>(
    rendered: &StackStr<N>,
    too_long: &'static str,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    if rendered.is_truncated() {
        return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", too_long));
    }
    arena.alloc_str(rendered.as_str()).map_err(|_| arena_full())
}
